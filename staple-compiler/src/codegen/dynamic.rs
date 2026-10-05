//! Memory operations shared by dynamic implementations and reabstraction.
use inkwell::{AddressSpace, values::PointerValue};

use super::{Backend, CodeGenerationResult, Diagnostic, Span, compiler_diagnostic};

impl<'program, 'context> Backend<'program, 'context> {
    /// Copy exactly the descriptor's store size. Packed products permit only
    /// byte alignment, even when a component's standalone alignment is larger.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
