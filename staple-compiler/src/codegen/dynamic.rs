//! Memory operations shared by dynamic implementations and reabstraction.
use inkwell::{AddressSpace, values::PointerValue};

use super::{Backend, CodeGenerationResult, Diagnostic, Span, compiler_diagnostic};

impl<'program, 'context> Backend<'program, 'context> {
    /// Allocate a descriptor-sized value in the prologue. Alignment is supplied
    /// at runtime, so reserve padding and align the resulting byte address.
    /// The active builder must still be in the entry block: layout loads must
    /// dominate the allocation, and loops must never repeat stack allocation.
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
        self.builder
            .build_int_to_ptr(
                aligned,
                self.context.ptr_type(AddressSpace::default()),
                "dynamic.value",
            )
            .map_err(compiler_diagnostic)
    }

    /// Copy exactly the descriptor's store size. Packed products permit only
    /// byte alignment, even when a component's standalone alignment is larger.
    pub(crate) fn copy_dynamic_value(
        &self,
        descriptor: PointerValue<'context>,
        source: PointerValue<'context>,
        destination: PointerValue<'context>,
    ) -> CodeGenerationResult<()> {
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
        let entry = self
            .builder
            .build_struct_gep(
                self.type_descriptor_type(0),
                descriptor,
                4,
                "descriptor.drop.slot",
            )
            .map_err(compiler_diagnostic)?;
        let drop = self
            .builder
            .build_load(pointer, entry, "descriptor.drop")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
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
