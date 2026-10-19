//! Memory operations shared by dynamic implementations and reabstraction.
use inkwell::{AddressSpace, values::PointerValue};

use super::{Backend, CodeGenerationResult, Diagnostic, Span, compiler_diagnostic};

impl<'program, 'context> Backend<'program, 'context> {
    /// Preserve a descriptor graph beyond the frame that built it. Compound
    /// descriptors may point at other stack-built descriptors, so copying
    /// only their header and component pointers is insufficient.
    pub(crate) fn copy_dynamic_descriptor(
        &self,
        descriptor: PointerValue<'context>,
        span: &Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        let function = match self.llvm_module.get_function("__staple_copy_descriptor") {
            Some(function) => function,
            None => {
                let function = self.llvm_module.add_function(
                    "__staple_copy_descriptor",
                    pointer.fn_type(&[pointer.into()], false),
                    Some(inkwell::module::Linkage::Internal),
                );
                let builder = self.context.create_builder();
                let entry = self.context.append_basic_block(function, "entry");
                let loop_ = self.context.append_basic_block(function, "components");
                let body = self.context.append_basic_block(function, "copy.component");
                let done = self.context.append_basic_block(function, "done");
                builder.position_at_end(entry);
                let source = function.get_first_param().unwrap().into_pointer_value();
                let count_slot = builder
                    .build_struct_gep(self.type_descriptor_type(0), source, 5, "count.slot")
                    .map_err(compiler_diagnostic)?;
                let count = builder
                    .build_load(self.context.i32_type(), count_slot, "count")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let count = builder
                    .build_int_z_extend(count, self.size_type, "component.count")
                    .map_err(compiler_diagnostic)?;
                let header = self.size_type.const_int(
                    self.target_data
                        .get_store_size(&self.type_descriptor_type(0)),
                    false,
                );
                let word = self
                    .size_type
                    .const_int(self.target_data.get_pointer_byte_size(None) as u64, false);
                let components = builder
                    .build_int_mul(count, word, "components.bytes")
                    .map_err(compiler_diagnostic)?;
                let size = builder
                    .build_int_add(header, components, "descriptor.bytes")
                    .map_err(compiler_diagnostic)?;
                let allocate = self
                    .llvm_module
                    .get_function("__staple_gc_alloc")
                    .ok_or_else(|| Diagnostic::new(span.clone(), "missing descriptor allocator"))?;
                let copy = builder
                    .build_direct_call(allocate, &[size.into()], "descriptor.copy")
                    .map_err(compiler_diagnostic)?
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_pointer_value();
                builder
                    .build_memcpy(
                        copy,
                        self.target_data
                            .get_abi_alignment(&self.type_descriptor_type(0)),
                        source,
                        self.target_data
                            .get_abi_alignment(&self.type_descriptor_type(0)),
                        size,
                    )
                    .map_err(compiler_diagnostic)?;
                builder
                    .build_unconditional_branch(loop_)
                    .map_err(compiler_diagnostic)?;
                builder.position_at_end(loop_);
                let index = builder
                    .build_phi(self.size_type, "component.index")
                    .map_err(compiler_diagnostic)?;
                index.add_incoming(&[(&self.size_type.const_zero(), entry)]);
                let current = index.as_basic_value().into_int_value();
                let more = builder
                    .build_int_compare(inkwell::IntPredicate::ULT, current, count, "has.component")
                    .map_err(compiler_diagnostic)?;
                builder
                    .build_conditional_branch(more, body, done)
                    .map_err(compiler_diagnostic)?;
                builder.position_at_end(body);
                let offset = builder
                    .build_int_mul(current, word, "component.offset")
                    .map_err(compiler_diagnostic)?;
                let offset = builder
                    .build_int_add(header, offset, "component.address")
                    .map_err(compiler_diagnostic)?;
                let source_slot = unsafe {
                    builder.build_gep(
                        self.context.i8_type(),
                        source,
                        &[offset],
                        "source.component",
                    )
                }
                .map_err(compiler_diagnostic)?;
                let child = builder
                    .build_load(pointer, source_slot, "child.descriptor")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let child_copy = builder
                    .build_direct_call(function, &[child.into()], "child.copy")
                    .map_err(compiler_diagnostic)?
                    .try_as_basic_value()
                    .unwrap_basic();
                let copy_slot = unsafe {
                    builder.build_gep(self.context.i8_type(), copy, &[offset], "copy.component")
                }
                .map_err(compiler_diagnostic)?;
                builder
                    .build_store(copy_slot, child_copy)
                    .map_err(compiler_diagnostic)?;
                let next = builder
                    .build_int_add(
                        current,
                        self.size_type.const_int(1, false),
                        "next.component",
                    )
                    .map_err(compiler_diagnostic)?;
                builder
                    .build_unconditional_branch(loop_)
                    .map_err(compiler_diagnostic)?;
                index.add_incoming(&[(&next, body)]);
                builder.position_at_end(done);
                builder
                    .build_return(Some(&copy))
                    .map_err(compiler_diagnostic)?;
                function
            }
        };
        self.builder
            .build_direct_call(function, &[descriptor.into()], "descriptor.copy")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic()
            .map(|value| value.into_pointer_value())
            .ok_or_else(|| Diagnostic::new(span.clone(), "descriptor copy has no result"))
    }

    /// Allocate a descriptor-sized value in the prologue. Alignment is supplied
    /// at runtime, so reserve padding and align the resulting byte address.
    /// The active builder must still be in the entry block: layout loads must
    /// dominate the allocation, and loops must never repeat stack allocation.
    /// The value starts zeroed: field stores never write product padding, and
    /// whole-value copies would otherwise carry stale stack bytes (including
    /// old pointers the conservative collector would retain) into the result.
    pub(crate) fn allocate_dynamic_value(
        &self,
        descriptor: PointerValue<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let block = self.builder.get_insert_block().ok_or_else(|| {
            Diagnostic::new(Span::Compiler, "dynamic allocation outside a function")
        })?;
        if block
            .get_parent()
            .and_then(|function| function.get_first_basic_block())
            != Some(block)
        {
            return Err(Diagnostic::new(
                Span::Compiler,
                "dynamic storage must be allocated in the entry block",
            ));
        }
        let layout = self.load_descriptor_layout(descriptor)?;
        let one = self.size_type.const_int(1, false);
        let padding = self
            .builder
            .build_int_sub(layout.align, one, "dynamic.padding")
            .map_err(compiler_diagnostic)?;
        let size = self
            .builder
            .build_int_add(layout.store_size, padding, "dynamic.capacity")
            .map_err(compiler_diagnostic)?;
        let allocation = self
            .builder
            .build_array_alloca(self.context.i8_type(), size, "dynamic.storage")
            .map_err(compiler_diagnostic)?;
        let address = self
            .builder
            .build_ptr_to_int(allocation, self.size_type, "dynamic.address")
            .map_err(compiler_diagnostic)?;
        let address = self
            .builder
            .build_int_add(address, padding, "dynamic.padded.address")
            .map_err(compiler_diagnostic)?;
        let units = self
            .builder
            .build_int_unsigned_div(address, layout.align, "dynamic.alignment.units")
            .map_err(compiler_diagnostic)?;
        let aligned = self
            .builder
            .build_int_mul(units, layout.align, "dynamic.aligned.address")
            .map_err(compiler_diagnostic)?;
        let value = self
            .builder
            .build_int_to_ptr(
                aligned,
                self.context.ptr_type(AddressSpace::default()),
                "dynamic.value",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_memset(
                value,
                1,
                self.context.i8_type().const_zero(),
                layout.store_size,
            )
            .map_err(compiler_diagnostic)?;
        Ok(value)
    }

    pub(crate) fn dynamic_byte_offset(
        &self,
        value: PointerValue<'context>,
        offset: inkwell::values::IntValue<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        unsafe {
            self.builder
                .build_gep(self.context.i8_type(), value, &[offset], "dynamic.offset")
        }
        .map_err(compiler_diagnostic)
    }

    pub(crate) fn descriptor_drop_pointer(
        &self,
        descriptor: PointerValue<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        let entry = self
            .builder
            .build_struct_gep(
                self.type_descriptor_type(0),
                descriptor,
                4,
                "descriptor.drop.slot",
            )
            .map_err(compiler_diagnostic)?;
        Ok(self
            .builder
            .build_load(pointer, entry, "descriptor.drop")
            .map_err(compiler_diagnostic)?
            .into_pointer_value())
    }

    /// Copy exactly the descriptor's store size. LLVM memcpy alignment must be
    /// a compile-time constant; the descriptor's runtime alignment cannot be used.
    /// Byte alignment also supports callers copying arbitrary byte regions.
    pub(crate) fn copy_dynamic_value(
        &self,
        descriptor: PointerValue<'context>,
        source: PointerValue<'context>,
        destination: PointerValue<'context>,
    ) -> CodeGenerationResult<()> {
        if source == destination {
            return Ok(());
        }
        let layout = self.load_descriptor_layout(descriptor)?;
        self.builder
            .build_memcpy(destination, 1, source, 1, layout.store_size)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// A descriptor with no cleanup entry is a no-op. Otherwise the drop entry
    /// receives its own descriptor so compound cleanup can read its components.
    pub(crate) fn drop_dynamic_value(
        &self,
        descriptor: PointerValue<'context>,
        value: PointerValue<'context>,
    ) -> CodeGenerationResult<()> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        let drop = self.descriptor_drop_pointer(descriptor)?;
        let present = self
            .builder
            .build_is_not_null(drop, "descriptor.has.drop")
            .map_err(compiler_diagnostic)?;
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "dynamic drop outside a function"))?;
        let cleanup = self.context.append_basic_block(function, "dynamic.drop");
        let continuation = self
            .context
            .append_basic_block(function, "dynamic.drop.done");
        self.builder
            .build_conditional_branch(present, cleanup, continuation)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(cleanup);
        let signature = self
            .context
            .void_type()
            .fn_type(&[pointer.into(), pointer.into()], false);
        self.builder
            .build_indirect_call(signature, drop, &[descriptor.into(), value.into()], "")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(continuation)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(continuation);
        Ok(())
    }
}
