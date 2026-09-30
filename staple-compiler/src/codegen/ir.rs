//! Stage 5.2: the backend-local pure-IR layer.
//!
//! Small IR constructs both emitters share: GC allocation, finalizer and root
//! registration, traps, unit values, and the byte helpers the UTF-8 validator
//! uses. Nothing here consults the checker or the `TypedModule`.

use inkwell::{
    AddressSpace,
    values::{AnyValue, AnyValueEnum},
};

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

    /// Legacy `build_owned_c_string`: a malloc'd NUL-terminated copy of one
    /// C-string payload. Shared (Stage 5.3 Step 5) so both emitters emit the
    /// same instructions under the same SSA names.
    pub(crate) fn build_owned_c_string(
        &self,
        value: &str,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let source = self
            .builder
            .build_global_string_ptr(value, "c_string.literal")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .as_pointer_value();
        let length = self
            .size_type
            .const_int((value.len() as u64).saturating_add(1), false);
        let pointer = self
            .builder
            .build_array_malloc(self.context.i8_type(), length, "c_string.data")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_memcpy(pointer, 1, source, 1, length)
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(pointer.as_any_value_enum())
    }

    /// Legacy `build_string_value`: the `{pointer, length}` slice value.
    pub(crate) fn build_string_value(
        &self,
        pointer: inkwell::values::PointerValue<'context>,
        length: inkwell::values::IntValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let mut value = self.slice_type().const_zero();
        value = self
            .builder
            .build_insert_value(value, pointer, 0, "string.pointer")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .into_struct_value();
        value = self
            .builder
            .build_insert_value(value, length, 1, "string.length")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .into_struct_value();
        Ok(value)
    }

    /// Legacy `compile_string_from_c_string`'s core (the caller evaluates the
    /// argument and, in legacy's case, releases the C string through its drop
    /// machinery): validate the C string as UTF-8, copy it to the GC heap, and
    /// return the owned String.
    pub(crate) fn build_string_from_c_string(
        &self,
        source: inkwell::values::PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let strlen_type = self.size_type.fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let strlen = self.declare_named_function("strlen", strlen_type);
        let length = self
            .builder
            .build_direct_call(strlen, &[source.into()], "c_string.length")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let validator = self
            .llvm_module
            .get_function("__staple_is_valid_utf8")
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing UTF-8 validator"))?;
        let valid = self
            .builder
            .build_direct_call(
                validator,
                &[source.into(), length.into()],
                "c_string.valid_utf8",
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let invalid = self
            .builder
            .build_not(valid, "c_string.invalid_utf8")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.build_trap_if(invalid, span.clone())?;
        let pointer = self.build_gc_allocation(length, "string.data", span.clone())?;
        self.builder
            .build_memcpy(pointer, 1, source, 1, length)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let result = self.build_string_value(pointer, length, span.clone())?;
        Ok(result)
    }

    /// The CString release call (`free`) both emitters emit for a consumed C
    /// string. Legacy reaches it through `compile_drop_value`, so its test-only
    /// drop-site recorder still sees the obligation; the lowered emitter calls
    /// it directly at the same site.
    pub(crate) fn build_free_c_string(
        &self,
        source: inkwell::values::PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let free_type = self.context.void_type().fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let free = self.declare_named_function("free", free_type);
        self.builder
            .build_direct_call(free, &[source.into()], "c_string.drop")
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(())
    }

    /// Legacy `compile_string_to_c_string`'s core (the caller evaluates the
    /// argument): trap on an interior NUL, allocate a NUL-terminated copy, and
    /// return it.
    pub(crate) fn build_string_to_c_string(
        &self,
        string: inkwell::values::StructValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let pointer = self
            .builder
            .build_extract_value(string, 0, "string.pointer")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .into_pointer_value();
        let length = self
            .builder
            .build_extract_value(string, 1, "string.length")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .into_int_value();
        let memchr_type = self.context.ptr_type(AddressSpace::default()).fn_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.context.i32_type().into(),
                self.size_type.into(),
            ],
            false,
        );
        let memchr = self.declare_named_function("memchr", memchr_type);
        let nul = self
            .builder
            .build_direct_call(
                memchr,
                &[
                    pointer.into(),
                    self.context.i32_type().const_zero().into(),
                    length.into(),
                ],
                "string.interior_nul",
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let has_nul = self
            .builder
            .build_is_not_null(nul, "string.has_interior_nul")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.build_trap_if(has_nul, span.clone())?;
        let allocation_length = self
            .builder
            .build_int_add(
                length,
                self.size_type.const_int(1, false),
                "c_string.length",
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let overflow = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                allocation_length,
                length,
                "c_string.length_overflow",
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.build_trap_if(overflow, span.clone())?;
        let result = self
            .builder
            .build_array_malloc(self.context.i8_type(), allocation_length, "c_string.data")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_memcpy(result, 1, pointer, 1, length)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let terminator = unsafe {
            self.builder.build_gep(
                self.context.i8_type(),
                result,
                &[length],
                "c_string.terminator",
            )
        }
        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_store(terminator, self.context.i8_type().const_zero())
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(result)
    }

    /// Legacy's integer binary builder with its SSA names
    /// (`{type}.add`/`.subtract`/`.multiply`/`.divide`).
    pub(crate) fn build_integer_binary(
        &self,
        integer: crate::IntegerType,
        operation: crate::IntegerBinaryOperation,
        left: inkwell::values::IntValue<'context>,
        right: inkwell::values::IntValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        use crate::IntegerBinaryOperation;
        let value = match operation {
            IntegerBinaryOperation::Add => self.builder.build_int_add(
                left,
                right,
                &format!("{}.add", integer.intrinsic_name()),
            ),
            IntegerBinaryOperation::Subtract => self.builder.build_int_sub(
                left,
                right,
                &format!("{}.subtract", integer.intrinsic_name()),
            ),
            IntegerBinaryOperation::Multiply => self.builder.build_int_mul(
                left,
                right,
                &format!("{}.multiply", integer.intrinsic_name()),
            ),
            IntegerBinaryOperation::Divide if integer.is_signed() => self
                .builder
                .build_int_signed_div(left, right, &format!("{}.divide", integer.intrinsic_name())),
            IntegerBinaryOperation::Divide => self.builder.build_int_unsigned_div(
                left,
                right,
                &format!("{}.divide", integer.intrinsic_name()),
            ),
        }
        .map_err(compiler_diagnostic)?;
        Ok(value)
    }

    /// Legacy's integer compare builder with its SSA name
    /// (`{type}.compare`).
    pub(crate) fn build_integer_compare(
        &self,
        integer: crate::IntegerType,
        operation: crate::IntegerCompareOperation,
        left: inkwell::values::IntValue<'context>,
        right: inkwell::values::IntValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        use crate::IntegerCompareOperation;
        let predicate = match (operation, integer.is_signed()) {
            (IntegerCompareOperation::Equal, _) => inkwell::IntPredicate::EQ,
            (IntegerCompareOperation::NotEqual, _) => inkwell::IntPredicate::NE,
            (IntegerCompareOperation::LessThan, true) => inkwell::IntPredicate::SLT,
            (IntegerCompareOperation::LessThan, false) => inkwell::IntPredicate::ULT,
            (IntegerCompareOperation::LessThanOrEqual, true) => inkwell::IntPredicate::SLE,
            (IntegerCompareOperation::LessThanOrEqual, false) => inkwell::IntPredicate::ULE,
            (IntegerCompareOperation::GreaterThan, true) => inkwell::IntPredicate::SGT,
            (IntegerCompareOperation::GreaterThan, false) => inkwell::IntPredicate::UGT,
            (IntegerCompareOperation::GreaterThanOrEqual, true) => inkwell::IntPredicate::SGE,
            (IntegerCompareOperation::GreaterThanOrEqual, false) => inkwell::IntPredicate::UGE,
        };
        self.builder
            .build_int_compare(
                predicate,
                left,
                right,
                &format!("{}.compare", integer.intrinsic_name()),
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
