//! Stage 5.2: the backend-local pure-IR layer.
//!
//! Small IR constructs both emitters share: GC allocation, finalizer and root
//! registration, traps, unit values, and the byte helpers the UTF-8 validator
//! uses. Nothing here consults the checker or the `TypedModule`.

use inkwell::{AddressSpace, values::AnyValue};

use super::{Backend, CodeGenerationResult, Diagnostic, Span, compiler_diagnostic};

impl<'program, 'context> Backend<'program, 'context> {
    /// Registers a GC heap region `[pointer, pointer + size)` as a root.
    pub(crate) fn register_gc_root_region(
        &self,
        pointer: inkwell::values::PointerValue<'context>,
        size: u64,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let register = self
            .llvm_module
            .get_function("__staple_gc_register_root")
            .expect("GC root registration function");
        self.builder
            .build_direct_call(
                register,
                &[pointer.into(), self.size_type.const_int(size, false).into()],
                "",
            )
            .map(|_| ())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// Registers an interior pointer so the collector keeps its allocation
    /// alive.
    pub(crate) fn register_gc_interior(
        &self,
        interior: inkwell::values::PointerValue<'context>,
        payload: inkwell::values::PointerValue<'context>,
    ) -> CodeGenerationResult<()> {
        let function_type = self.context.void_type().fn_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.context.ptr_type(AddressSpace::default()).into(),
            ],
            false,
        );
        let register = self
            .llvm_module
            .get_function("__staple_gc_register_interior")
            .unwrap_or_else(|| {
                self.llvm_module
                    .add_function("__staple_gc_register_interior", function_type, None)
            });
        self.builder
            .build_direct_call(register, &[interior.into(), payload.into()], "")
            .map(|_| ())
            .map_err(compiler_diagnostic)
    }

    /// Allocates `size` bytes from the GC heap.
    pub(crate) fn build_gc_allocation(
        &self,
        size: inkwell::values::IntValue<'context>,
        name: &str,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let allocator = self
            .llvm_module
            .get_function("__staple_gc_alloc")
            .unwrap_or_else(|| {
                let function_type = self
                    .context
                    .ptr_type(AddressSpace::default())
                    .fn_type(&[self.size_type.into()], false);
                self.llvm_module
                    .add_function("__staple_gc_alloc", function_type, None)
            });
        let pointer = self
            .builder
            .build_direct_call(allocator, &[size.into()], name)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        Ok(pointer)
    }

    /// Attaches a finalizer to a GC allocation.
    pub(crate) fn set_gc_finalizer(
        &self,
        pointer: inkwell::values::PointerValue<'context>,
        finalizer: inkwell::values::FunctionValue<'context>,
    ) -> CodeGenerationResult<()> {
        let setter_type = self.context.void_type().fn_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.context.ptr_type(AddressSpace::default()).into(),
            ],
            false,
        );
        let setter = self
            .llvm_module
            .get_function("__staple_gc_set_finalizer")
            .unwrap_or_else(|| {
                self.llvm_module
                    .add_function("__staple_gc_set_finalizer", setter_type, None)
            });
        self.builder
            .build_direct_call(
                setter,
                &[
                    pointer.into(),
                    finalizer.as_global_value().as_pointer_value().into(),
                ],
                "gc.finalizer",
            )
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Branches to an `llvm.trap` block when `condition` holds, continuing
    /// otherwise.
    pub(crate) fn build_trap_if(
        &self,
        condition: inkwell::values::IntValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let current = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "trap has no containing function"))?;
        let trap_block = self.context.append_basic_block(current, "trap");
        let continue_block = self.context.append_basic_block(current, "trap.continue");
        self.builder
            .build_conditional_branch(condition, trap_block, continue_block)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder.position_at_end(trap_block);
        let trap = self
            .llvm_module
            .get_function("llvm.trap")
            .unwrap_or_else(|| {
                self.llvm_module.add_function(
                    "llvm.trap",
                    self.context.void_type().fn_type(&[], false),
                    None,
                )
            });
        self.builder
            .build_direct_call(trap, &[], "")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_unreachable()
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder.position_at_end(continue_block);
        Ok(())
    }

    pub(crate) fn unit_value(&self) -> inkwell::values::AnyValueEnum<'context> {
        self.context
            .struct_type(&[], true)
            .const_zero()
            .as_any_value_enum()
    }

    pub(crate) fn increment_utf8_index(
        &self,
        slot: inkwell::values::PointerValue<'context>,
        index: inkwell::values::IntValue<'context>,
        destination: inkwell::basic_block::BasicBlock<'context>,
    ) -> CodeGenerationResult<()> {
        let next = self
            .builder
            .build_int_add(index, self.size_type.const_int(1, false), "next_index")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(slot, next)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(destination)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    pub(crate) fn byte_in_range(
        &self,
        byte: inkwell::values::IntValue<'context>,
        minimum: u64,
        maximum: u64,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        let ty = self.context.i8_type();
        let lower = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGE,
                byte,
                ty.const_int(minimum, false),
                "byte.lower",
            )
            .map_err(compiler_diagnostic)?;
        let upper = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULE,
                byte,
                ty.const_int(maximum, false),
                "byte.upper",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_and(lower, upper, "byte.in_range")
            .map_err(compiler_diagnostic)
    }

    pub(crate) fn byte_equals(
        &self,
        byte: inkwell::values::IntValue<'context>,
        expected: u64,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        self.builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                byte,
                self.context.i8_type().const_int(expected, false),
                "byte.equals",
            )
            .map_err(compiler_diagnostic)
    }
}

pub(crate) fn value_as_basic(
    value: inkwell::values::AnyValueEnum<'_>,
) -> Option<inkwell::values::BasicValueEnum<'_>> {
    use inkwell::values::AnyValueEnum;
    match value {
        AnyValueEnum::ArrayValue(value) => Some(value.into()),
        AnyValueEnum::FloatValue(value) => Some(value.into()),
        AnyValueEnum::FunctionValue(value) => {
            Some(value.as_global_value().as_pointer_value().into())
        }
        AnyValueEnum::IntValue(value) => Some(value.into()),
        AnyValueEnum::PointerValue(value) => Some(value.into()),
        AnyValueEnum::StructValue(value) => Some(value.into()),
        AnyValueEnum::VectorValue(value) => Some(value.into()),
        _ => None,
    }
}
