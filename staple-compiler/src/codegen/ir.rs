//! Stage 5.2: the backend-local pure-IR layer.
//!
//! Small IR constructs both emitters share: GC allocation, finalizer and root
//! registration, traps, unit values, and the byte helpers the UTF-8 validator
//! uses. Nothing here consults the checker or the `TypedModule`.

use inkwell::{
    AddressSpace,
    basic_block::BasicBlock,
    types::{BasicTypeEnum, StructType},
    values::{AnyValue, AnyValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue},
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

    /// Legacy's String literal: copy a global byte string to a GC allocation
    /// and build the `{pointer, length}` value. Shared by both emitters (5.4
    /// Step 3); the caller decodes the literal text.
    pub(crate) fn build_string_literal(
        &self,
        value: &str,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let source = self
            .builder
            .build_global_string_ptr(value, "string")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .as_pointer_value();
        let length = self.size_type.const_int(value.len() as u64, false);
        let pointer = self.build_gc_allocation(length, "string.data", span.clone())?;
        self.builder
            .build_memcpy(pointer, 1, source, 1, length)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.build_string_value(pointer, length, span)
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

    /// Legacy's float binary builder with its SSA names
    /// (`{type}.add`/`.subtract`/`.multiply`/`.divide`). Stage 5.4 Step 2:
    /// both emitters share it, so the lower emitter's `FloatBinary` output is
    /// identical to legacy's.
    pub(crate) fn build_float_binary(
        &self,
        float: crate::FloatType,
        operation: crate::IntegerBinaryOperation,
        left: inkwell::values::FloatValue<'context>,
        right: inkwell::values::FloatValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::FloatValue<'context>> {
        use crate::IntegerBinaryOperation;
        let name = format!(
            "{}.{}",
            float.intrinsic_name(),
            match operation {
                IntegerBinaryOperation::Add => "add",
                IntegerBinaryOperation::Subtract => "subtract",
                IntegerBinaryOperation::Multiply => "multiply",
                IntegerBinaryOperation::Divide => "divide",
            }
        );
        let value = match operation {
            IntegerBinaryOperation::Add => self.builder.build_float_add(left, right, &name),
            IntegerBinaryOperation::Subtract => self.builder.build_float_sub(left, right, &name),
            IntegerBinaryOperation::Multiply => self.builder.build_float_mul(left, right, &name),
            IntegerBinaryOperation::Divide => self.builder.build_float_div(left, right, &name),
        }
        .map_err(compiler_diagnostic)?;
        Ok(value)
    }

    /// Legacy's float compare builder with its SSA name (`{type}.compare`).
    pub(crate) fn build_float_compare(
        &self,
        float: crate::FloatType,
        operation: crate::IntegerCompareOperation,
        left: inkwell::values::FloatValue<'context>,
        right: inkwell::values::FloatValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        use crate::IntegerCompareOperation;
        let predicate = match operation {
            IntegerCompareOperation::Equal => inkwell::FloatPredicate::OEQ,
            IntegerCompareOperation::NotEqual => inkwell::FloatPredicate::UNE,
            IntegerCompareOperation::LessThan => inkwell::FloatPredicate::OLT,
            IntegerCompareOperation::LessThanOrEqual => inkwell::FloatPredicate::OLE,
            IntegerCompareOperation::GreaterThan => inkwell::FloatPredicate::OGT,
            IntegerCompareOperation::GreaterThanOrEqual => inkwell::FloatPredicate::OGE,
        };
        self.builder
            .build_float_compare(
                predicate,
                left,
                right,
                &format!("{}.compare", float.intrinsic_name()),
            )
            .map_err(compiler_diagnostic)
    }

    /// Legacy's Bool builder: tag 0 (`True`) or 1 (`False`) in the two-arm sum.
    pub(crate) fn build_bool_value(
        &self,
        condition: inkwell::values::IntValue<'context>,
        sum_type: inkwell::types::StructType<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::AnyValueEnum<'context>> {
        // `Bool` is declared as `True | False` in the standard-library contract.
        let true_index = 0;
        let false_index = 1;
        let tag = self
            .builder
            .build_select(
                condition,
                self.context.i32_type().const_int(true_index as u64, false),
                self.context.i32_type().const_int(false_index as u64, false),
                "bool.tag",
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_insert_value(sum_type.const_zero(), tag, 0, "bool.value")
            .map(|value| value.as_any_value_enum())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// Legacy's symbol-initialization check: load the state byte and trap
    /// unless it is 2. Both emitters share it (5.4 Step 2).
    pub(crate) fn build_initialization_check(
        &self,
        state_slot: inkwell::values::PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let state = self
            .builder
            .build_load(self.context.i8_type(), state_slot, "binding.state")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .into_int_value();
        let invalid = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                state,
                self.context.i8_type().const_int(2, false),
                "binding.uninitialized",
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.build_trap_if(invalid, span)
    }

    /// Legacy's bounds-checked element pointer: trap when `position >= length`,
    /// then GEP. Shared by slice reads and other index paths.
    pub(crate) fn build_index_pointer(
        &self,
        pointer: inkwell::values::PointerValue<'context>,
        position: inkwell::values::IntValue<'context>,
        length: inkwell::values::IntValue<'context>,
        element_type: inkwell::types::BasicTypeEnum<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let out_of_bounds = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGE,
                position,
                length,
                "index.out_of_bounds",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(out_of_bounds, span)?;
        unsafe {
            self.builder
                .build_gep(element_type, pointer, &[position], "index.element")
        }
        .map_err(compiler_diagnostic)
    }

    /// Legacy `SliceLength`'s core: field 1 of the `{pointer, length}` slice.
    pub(crate) fn build_slice_length(
        &self,
        slice: inkwell::values::StructValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        self.builder
            .build_extract_value(slice, 1, "slice.length")
            .map(|value| value.into_int_value())
            .map_err(compiler_diagnostic)
    }

    /// Legacy `SliceGetRef`'s core: extract the slice's pointer and length and
    /// return the bounds-checked element pointer.
    pub(crate) fn build_slice_get_ref(
        &self,
        slice: inkwell::values::StructValue<'context>,
        position: inkwell::values::IntValue<'context>,
        element_type: inkwell::types::BasicTypeEnum<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let pointer = self
            .builder
            .build_extract_value(slice, 0, "slice.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let length = self
            .builder
            .build_extract_value(slice, 1, "slice.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        self.build_index_pointer(pointer, position, length, element_type, span)
    }

    /// Legacy's `{code, environment}` closure value.
    pub(crate) fn build_closure_value(
        &self,
        code: inkwell::values::FunctionValue<'context>,
        environment: inkwell::values::PointerValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let mut closure = self.closure_type().const_zero();
        closure = self
            .builder
            .build_insert_value(
                closure,
                code.as_global_value().as_pointer_value(),
                0,
                "closure.code",
            )
            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?
            .into_struct_value();
        self.builder
            .build_insert_value(closure, environment, 1, "closure.environment")
            .map(|value| value.into_struct_value())
            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))
    }

    /// The capture-environment struct layout: one field per capture, in capture
    /// order. `fields` are the per-capture storage types (`capture_field_type`
    /// in the lowered emitter, legacy `compile_capture_type`).
    pub(crate) fn capture_environment_type(
        &self,
        fields: &[inkwell::types::BasicTypeEnum<'context>],
    ) -> inkwell::types::StructType<'context> {
        self.context.struct_type(fields, false)
    }

    /// One capture field insert, always named `capture` like legacy's.
    pub(crate) fn insert_capture(
        &self,
        environment: inkwell::values::StructValue<'context>,
        value: inkwell::values::BasicValueEnum<'context>,
        index: u32,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        self.builder
            .build_insert_value(environment, value, index, "capture")
            .map(|value| value.into_struct_value())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// GC-allocate one built capture environment and store it, like legacy
    /// `build_capture_environment`'s allocation tail.
    pub(crate) fn allocate_capture_environment(
        &self,
        environment_type: inkwell::types::StructType<'context>,
        environment_value: inkwell::values::StructValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let size = self.target_data.get_store_size(&environment_type);
        let pointer = self.build_gc_allocation(
            self.size_type.const_int(size, false),
            "closure.environment",
            span.clone(),
        )?;
        self.builder
            .build_store(pointer, environment_value)
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(pointer)
    }

    /// Legacy `build_product_value`: build a literal struct from the given
    /// values (a single value is returned unchanged; the empty product is a
    /// zero-sized struct only when the caller passes no values). Shared by the
    /// constructor and product paths.
    pub(crate) fn build_product_value(
        &self,
        values: &[inkwell::values::BasicValueEnum<'context>],
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::BasicValueEnum<'context>> {
        if let [value] = values {
            return Ok(*value);
        }
        let types = values
            .iter()
            .map(inkwell::values::BasicValueEnum::get_type)
            .collect::<Vec<_>>();
        let mut product = self.context.struct_type(&types, true).const_zero();
        for (index, value) in values.iter().copied().enumerate() {
            product = self
                .builder
                .build_insert_value(product, value, index as u32, "product.element")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                .into_struct_value();
        }
        Ok(product.into())
    }

    /// The computed temporary legacy's indirect/mutation argument paths
    /// materialize: alloca the parameter's concrete type and store the value.
    pub(crate) fn build_argument_temporary(
        &self,
        value: inkwell::values::BasicValueEnum<'context>,
        llvm_type: inkwell::types::BasicTypeEnum<'context>,
        name: &str,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let pointer = self
            .builder
            .build_alloca(llvm_type, name)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(pointer, value)
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(pointer)
    }

    /// Legacy `compile_numeric_to_string`'s core (the caller evaluates the
    /// argument): format the value into a stack buffer and copy it to a GC
    /// String.
    pub(crate) fn build_numeric_to_string(
        &self,
        numeric: crate::NumericType,
        argument: inkwell::values::BasicValueEnum<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::AnyValueEnum<'context>> {
        let capacity = self.context.i32_type().const_int(128, false);
        let buffer = self
            .builder
            .build_array_alloca(self.context.i8_type(), capacity, "to_string.buffer")
            .map_err(compiler_diagnostic)?;
        let format = match numeric {
            crate::NumericType::Integer(integer) if integer.is_signed() => "%lld",
            crate::NumericType::Integer(_) => "%llu",
            crate::NumericType::Float(crate::FloatType::F32) => "%.9g",
            crate::NumericType::Float(crate::FloatType::F64) => "%.17g",
        };
        let format = self
            .builder
            .build_global_string_ptr(format, "to_string.format")
            .map_err(compiler_diagnostic)?
            .as_pointer_value();
        let formatted = match (numeric, argument) {
            (
                crate::NumericType::Integer(integer),
                inkwell::values::BasicValueEnum::IntValue(value),
            ) => {
                let bits = value.get_type().get_bit_width();
                let wide = if bits < 64 {
                    if integer.is_signed() {
                        self.builder.build_int_s_extend(
                            value,
                            self.context.i64_type(),
                            "to_string.integer",
                        )
                    } else {
                        self.builder.build_int_z_extend(
                            value,
                            self.context.i64_type(),
                            "to_string.integer",
                        )
                    }
                    .map_err(compiler_diagnostic)?
                } else {
                    value
                };
                inkwell::values::BasicValueEnum::IntValue(wide)
            }
            (
                crate::NumericType::Float(crate::FloatType::F32),
                inkwell::values::BasicValueEnum::FloatValue(value),
            ) => inkwell::values::BasicValueEnum::FloatValue(
                self.builder
                    .build_float_ext(value, self.context.f64_type(), "to_string.float")
                    .map_err(compiler_diagnostic)?,
            ),
            (
                crate::NumericType::Float(crate::FloatType::F64),
                inkwell::values::BasicValueEnum::FloatValue(value),
            ) => inkwell::values::BasicValueEnum::FloatValue(value),
            _ => {
                return Err(Diagnostic::new(
                    span,
                    "numeric conversion requires a numeric value",
                ));
            }
        };
        let snprintf_type = self.context.i32_type().fn_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.size_type.into(),
                self.context.ptr_type(AddressSpace::default()).into(),
            ],
            true,
        );
        let snprintf = self.declare_named_function("snprintf", snprintf_type);
        let length = self
            .builder
            .build_direct_call(
                snprintf,
                &[
                    buffer.into(),
                    self.size_type.const_int(128, false).into(),
                    format.into(),
                    formatted.into(),
                ],
                "to_string.length",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let length = self
            .builder
            .build_int_z_extend(length, self.size_type, "to_string.size")
            .map_err(compiler_diagnostic)?;
        let pointer = self.build_gc_allocation(length, "to_string.data", span.clone())?;
        self.builder
            .build_memcpy(pointer, 1, buffer, 1, length)
            .map_err(compiler_diagnostic)?;
        Ok(self
            .build_string_value(pointer, length, span)?
            .as_any_value_enum())
    }

    /// Legacy `compile_string_add`'s core (the caller evaluates the operands):
    /// concatenate two Strings into one GC allocation, with the same overflow
    /// check and SSA names.
    pub(crate) fn build_string_add(
        &self,
        left: inkwell::values::StructValue<'context>,
        right: inkwell::values::StructValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::AnyValueEnum<'context>> {
        let left_pointer = self
            .builder
            .build_extract_value(left, 0, "string.add.left.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let left_length = self
            .builder
            .build_extract_value(left, 1, "string.add.left.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let right_pointer = self
            .builder
            .build_extract_value(right, 0, "string.add.right.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let right_length = self
            .builder
            .build_extract_value(right, 1, "string.add.right.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let length = self
            .builder
            .build_int_add(left_length, right_length, "string.add.length")
            .map_err(compiler_diagnostic)?;
        let overflow = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                length,
                left_length,
                "string.add.overflow",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(overflow, span.clone())?;
        let pointer = self.build_gc_allocation(length, "string.add.data", span.clone())?;
        self.builder
            .build_memcpy(pointer, 1, left_pointer, 1, left_length)
            .map_err(compiler_diagnostic)?;
        let right_target = unsafe {
            self.builder.build_gep(
                self.context.i8_type(),
                pointer,
                &[left_length],
                "string.add.right.target",
            )
        }
        .map_err(compiler_diagnostic)?;
        self.builder
            .build_memcpy(right_target, 1, right_pointer, 1, right_length)
            .map_err(compiler_diagnostic)?;
        Ok(self
            .build_string_value(pointer, length, span)?
            .as_any_value_enum())
    }

    /// Loads the value reached by following `payloads` (outermost first),
    /// loading the final payload as well. Stage 5.5 Step 4 shares it with the
    /// lowered `Ref` access and nominal pattern paths.
    pub(crate) fn load_ref_payloads(
        &self,
        value: inkwell::values::AnyValueEnum<'context>,
        payloads: &[crate::CheckedType],
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::BasicValueEnum<'context>> {
        let mut value = value_as_basic(value);
        for payload in payloads {
            let Some(inkwell::values::BasicValueEnum::PointerValue(pointer)) = value else {
                return Err(Diagnostic::new(
                    span.clone(),
                    "Ref value has an invalid representation",
                ));
            };
            let payload_type = self.compile_type(payload)?;
            value = Some(
                self.builder
                    .build_load(payload_type, pointer, "ref.payload")
                    .map_err(compiler_diagnostic)?,
            );
        }
        value.ok_or_else(|| Diagnostic::new(span, "Ref value has an invalid representation"))
    }

    /// The address of the payload reached by following `payloads`, leaving
    /// the final payload in place rather than loading it.
    pub(crate) fn ref_payload_pointer(
        &self,
        value: inkwell::values::AnyValueEnum<'context>,
        payloads: &[crate::CheckedType],
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let mut pointer = match value_as_basic(value) {
            Some(inkwell::values::BasicValueEnum::PointerValue(pointer)) => pointer,
            _ => {
                return Err(Diagnostic::new(
                    span,
                    "Ref value has an invalid representation",
                ));
            }
        };
        for payload in &payloads[..payloads.len().saturating_sub(1)] {
            pointer = self
                .builder
                .build_load(self.compile_type(payload)?, pointer, "ref.payload")
                .map_err(compiler_diagnostic)?
                .into_pointer_value();
        }
        Ok(pointer)
    }

    /// Stage 5.5 Step 3: field 0 of a sum representation (the `i32` tag).
    pub(crate) fn build_sum_tag(
        &self,
        value: inkwell::values::StructValue<'context>,
        name: &str,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        self.builder
            .build_extract_value(value, 0, name)
            .map(|value| value.into_int_value())
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.5 Step 3: compare a sum tag against one alternative index.
    pub(crate) fn build_sum_tag_compare(
        &self,
        tag: inkwell::values::IntValue<'context>,
        index: usize,
        name: &str,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        self.builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                tag,
                self.context.i32_type().const_int(index as u64, false),
                name,
            )
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.5 Step 3: the alloca a sum coercion stores its result into,
    /// with the tag/payload projection legacy `coerce_sum_value` builds.
    pub(crate) fn begin_sum_storage(
        &self,
        sum: &crate::CheckedSumType,
        span: &Span,
    ) -> CodeGenerationResult<SumStorageSlot<'context>> {
        let llvm_type = self.compile_sum_type(sum)?;
        let slot = self
            .builder
            .build_alloca(llvm_type, "sum.target")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_store(slot, llvm_type.const_zero())
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let tag = self
            .builder
            .build_struct_gep(llvm_type, slot, 0, "sum.target.tag")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let payload = self
            .builder
            .build_struct_gep(llvm_type, slot, 1, "sum.target.payload")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let payload_type = llvm_type
            .get_field_type_at_index(1)
            .expect("sum payload field");
        let alignment = self.target_data.get_abi_alignment(&payload_type);
        Ok(SumStorageSlot {
            llvm_type,
            slot,
            storage: super::layout::SumStorage {
                tag,
                payload,
                alignment,
            },
        })
    }

    /// Stage 5.5 Step 3: load the finished sum value out of its storage slot.
    pub(crate) fn load_sum_storage(
        &self,
        storage: &SumStorageSlot<'context>,
        span: &Span,
    ) -> CodeGenerationResult<inkwell::values::AnyValueEnum<'context>> {
        self.builder
            .build_load(storage.llvm_type, storage.slot, "sum.value")
            .map(|value| value.as_any_value_enum())
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))
    }

    /// Stage 5.5 Step 3: legacy `store_sum_payload`'s core — write the tag and
    /// memcpy the alternative into the payload field.
    pub(crate) fn store_sum_payload(
        &self,
        value: inkwell::values::AnyValueEnum<'context>,
        value_type: &crate::CheckedType,
        index: usize,
        storage: &super::layout::SumStorage<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        self.builder
            .build_store(
                storage.tag,
                self.context.i32_type().const_int(index as u64, false),
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let source_type = self.compile_type(value_type)?;
        let source_value = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(span.clone(), "sum alternative is not a first-class value")
        })?;
        let source_slot = self
            .builder
            .build_alloca(source_type, "sum.source")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_store(source_slot, source_value)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let size = self
            .size_type
            .const_int(self.target_data.get_store_size(&source_type), false);
        self.builder
            .build_memcpy(
                storage.payload,
                storage.alignment,
                source_slot,
                self.target_data.get_abi_alignment(&source_type),
                size,
            )
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(())
    }

    /// Stage 5.5 Step 3: legacy `extract_sum_alternative`'s core — memcpy the
    /// payload out and reinterpret it as the alternative type.
    pub(crate) fn extract_sum_alternative(
        &self,
        value: inkwell::values::StructValue<'context>,
        sum: &crate::CheckedSumType,
        index: usize,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::BasicValueEnum<'context>> {
        let sum_type = self.compile_sum_type(sum)?;
        let sum_slot = self
            .builder
            .build_alloca(sum_type, "sum.extract.source")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_store(sum_slot, value)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let payload = self
            .builder
            .build_struct_gep(sum_type, sum_slot, 1, "sum.extract.payload")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let payload_type = sum_type
            .get_field_type_at_index(1)
            .expect("sum payload field");
        let alternative = sum.alternatives.get(index).ok_or_else(|| {
            Diagnostic::new(span.clone(), "sum alternative index is out of bounds")
        })?;
        let alternative_type = self.compile_type(alternative)?;
        let alternative_slot = self
            .builder
            .build_alloca(alternative_type, "sum.extract.value")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_memcpy(
                alternative_slot,
                self.target_data.get_abi_alignment(&alternative_type),
                payload,
                self.target_data.get_abi_alignment(&payload_type),
                self.size_type
                    .const_int(self.target_data.get_store_size(&alternative_type), false),
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_load(alternative_type, alternative_slot, "sum.extract.result")
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// Stage 5.5 Step 3: legacy `coerce_slice_ref_value`'s core — build a
    /// `{pointer, length}` slice from a fixed-reference pointer.
    pub(crate) fn build_slice_ref_value(
        &self,
        pointer: inkwell::values::PointerValue<'context>,
        length: usize,
    ) -> CodeGenerationResult<inkwell::values::AnyValueEnum<'context>> {
        let mut result = self.slice_type().const_zero();
        result = self
            .builder
            .build_insert_value(result, pointer, 0, "slice.pointer")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        result = self
            .builder
            .build_insert_value(
                result,
                self.size_type.const_int(length as u64, false),
                1,
                "slice.length",
            )
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        Ok(result.as_any_value_enum())
    }

    /// Stage 5.5 Step 3: legacy `compile_string_literal_pattern_branch`'s core
    /// — compare the string fields against a literal with a length check and
    /// `memcmp`, branching to `success` or `failure`.
    pub(crate) fn build_string_literal_pattern_compare(
        &self,
        value: inkwell::values::StructValue<'context>,
        literal: &str,
        success: inkwell::basic_block::BasicBlock<'context>,
        failure: inkwell::basic_block::BasicBlock<'context>,
        _span: Span,
    ) -> CodeGenerationResult<()> {
        let pointer = self
            .builder
            .build_extract_value(value, 0, "match.string.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let length = self
            .builder
            .build_extract_value(value, 1, "match.string.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let expected_length = self.size_type.const_int(literal.len() as u64, false);
        let length_matches = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                length,
                expected_length,
                "match.string.length_matches",
            )
            .map_err(compiler_diagnostic)?;
        let compare = self.context.append_basic_block(
            success.get_parent().expect("match function"),
            "match.string.compare",
        );
        self.builder
            .build_conditional_branch(length_matches, compare, failure)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(compare);
        let expected = self
            .builder
            .build_global_string_ptr(literal, "match.string.literal")
            .map_err(compiler_diagnostic)?
            .as_pointer_value();
        let memcmp_type = self.context.i32_type().fn_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.context.ptr_type(AddressSpace::default()).into(),
                self.size_type.into(),
            ],
            false,
        );
        let memcmp = self.declare_named_function("memcmp", memcmp_type);
        let comparison = self
            .builder
            .build_direct_call(
                memcmp,
                &[pointer.into(), expected.into(), expected_length.into()],
                "match.string.bytes",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let matches = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                comparison,
                self.context.i32_type().const_zero(),
                "match.string.matches",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(matches, success, failure)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Shared bounds-checked structural load.
    pub(crate) fn build_index_load(
        &self,
        pointer: PointerValue<'context>,
        position: IntValue<'context>,
        length: IntValue<'context>,
        element: &crate::CheckedType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let element_type = self.compile_type(element)?;
        let pointer =
            self.build_index_pointer(pointer, position, length, element_type, span.clone())?;
        self.builder
            .build_load(element_type, pointer, "index.value")
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// Shared heterogeneous product dispatch; the emitter supplies recorded coercions.
    pub(crate) fn begin_structural_index(
        &self,
        position: IntValue<'context>,
        length: usize,
        output: &crate::CheckedType,
        span: Span,
    ) -> CodeGenerationResult<(
        BasicTypeEnum<'context>,
        PointerValue<'context>,
        BasicBlock<'context>,
        Vec<(IntValue<'context>, BasicBlock<'context>)>,
    )> {
        let llvm_length = self.size_type.const_int(length as u64, false);
        let out = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGE,
                position,
                llvm_length,
                "index.out_of_bounds",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(out, span)?;
        let output_type = self.compile_type(output)?;
        let output_slot = self
            .builder
            .build_alloca(output_type, "index.result")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(output_slot, output_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .expect("structural index is in a function");
        let merge = self.context.append_basic_block(function, "index.done");
        let cases = (0..length)
            .map(|index| {
                (
                    self.size_type.const_int(index as u64, false),
                    self.context.append_basic_block(function, "index.case"),
                )
            })
            .collect::<Vec<_>>();
        self.builder
            .build_switch(position, merge, &cases)
            .map_err(compiler_diagnostic)?;
        Ok((output_type, output_slot, merge, cases))
    }

    /// Rebuild a source product and pair it with its initial cursor.
    pub(crate) fn build_structural_iterator(
        &self,
        values: &[BasicValueEnum<'context>],
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let source_value = self.build_product_value(values, span.clone())?;
        let cursor = self.size_type.const_int(0, false);
        self.build_product_value(&[source_value, cursor.into()], span)
    }

    /// Shared Done/Yield control-flow and result storage.
    pub(crate) fn begin_structural_next(
        &self,
        cursor: IntValue<'context>,
        length: usize,
        result: &crate::CheckedType,
    ) -> CodeGenerationResult<(
        BasicTypeEnum<'context>,
        PointerValue<'context>,
        BasicBlock<'context>,
        BasicBlock<'context>,
        BasicBlock<'context>,
        BasicBlock<'context>,
    )> {
        let llvm_length = self.size_type.const_int(length as u64, false);
        let in_range = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                cursor,
                llvm_length,
                "next.in_range",
            )
            .map_err(compiler_diagnostic)?;

        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .expect("structural next is in a function");
        let done_block = self.context.append_basic_block(function, "next.done");
        let dispatch_block = self.context.append_basic_block(function, "next.dispatch");
        let unreachable_block = self
            .context
            .append_basic_block(function, "next.unreachable");
        let merge = self.context.append_basic_block(function, "next.merge");

        let result_type = self.compile_type(result)?;
        let result_slot = self
            .builder
            .build_alloca(result_type, "next.result")
            .map_err(compiler_diagnostic)?;

        self.builder
            .build_conditional_branch(in_range, dispatch_block, done_block)
            .map_err(compiler_diagnostic)?;

        Ok((
            result_type,
            result_slot,
            done_block,
            dispatch_block,
            unreachable_block,
            merge,
        ))
    }

    /// Dispatch to one recorded product element; impossible cursors are unreachable.
    pub(crate) fn begin_next_dispatch(
        &self,
        cursor: IntValue<'context>,
        length: usize,
        unreachable_block: BasicBlock<'context>,
    ) -> CodeGenerationResult<Vec<(IntValue<'context>, BasicBlock<'context>)>> {
        let function = unreachable_block
            .get_parent()
            .expect("structural next is in a function");
        let cases = (0..length)
            .map(|index| {
                (
                    self.size_type.const_int(index as u64, false),
                    self.context.append_basic_block(function, "next.case"),
                )
            })
            .collect::<Vec<_>>();
        self.builder
            .build_switch(cursor, unreachable_block, &cases)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(unreachable_block);
        self.builder
            .build_unreachable()
            .map_err(compiler_diagnostic)?;

        Ok(cases)
    }

    /// Shared null-environment call used by structural Debug delegates.
    pub(crate) fn build_debug_delegate(
        &self,
        function: inkwell::values::FunctionValue<'context>,
        value: BasicValueEnum<'context>,
        formatter: BasicValueEnum<'context>,
        name: &str,
    ) -> CodeGenerationResult<()> {
        self.builder
            .build_direct_call(
                function,
                &[
                    self.context
                        .ptr_type(AddressSpace::default())
                        .const_null()
                        .into(),
                    value.into(),
                    formatter.into(),
                ],
                name,
            )
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// The shared sum Debug dispatch skeleton.
    pub(crate) fn begin_debug_sum(
        &self,
        value: inkwell::values::StructValue<'context>,
        length: usize,
        span: Span,
    ) -> CodeGenerationResult<(
        inkwell::basic_block::BasicBlock<'context>,
        Vec<inkwell::basic_block::BasicBlock<'context>>,
    )> {
        let tag = self
            .builder
            .build_extract_value(value, 0, "debug.sum.tag")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span, "sum Debug is not in a function"))?;
        let merge = self.context.append_basic_block(function, "debug.sum.done");
        let cases = (0..length)
            .map(|index| {
                (
                    self.context.i32_type().const_int(index as u64, false),
                    self.context.append_basic_block(function, "debug.sum.case"),
                )
            })
            .collect::<Vec<_>>();
        self.builder
            .build_switch(tag, merge, &cases)
            .map_err(compiler_diagnostic)?;
        Ok((merge, cases.into_iter().map(|(_, block)| block).collect()))
    }

    /// Stage 5.5 Step 3: the literal half of legacy
    /// `compile_formatter_write_literal` once the target function is bound:
    /// allocate a `String` from the literal and call `Formatter.write`.
    pub(crate) fn build_formatter_write_literal(
        &self,
        function: inkwell::values::FunctionValue<'context>,
        formatter: inkwell::values::BasicValueEnum<'context>,
        literal: &str,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let source = self
            .builder
            .build_global_string_ptr(literal, "debug.literal")
            .map_err(compiler_diagnostic)?
            .as_pointer_value();
        let length = self.size_type.const_int(literal.len() as u64, false);
        let pointer = self.build_gc_allocation(length, "debug.literal.data", span.clone())?;
        self.builder
            .build_memcpy(pointer, 1, source, 1, length)
            .map_err(compiler_diagnostic)?;
        let string = self.build_string_value(pointer, length, span)?;
        self.builder
            .build_direct_call(
                function,
                &[
                    self.context
                        .ptr_type(AddressSpace::default())
                        .const_null()
                        .into(),
                    formatter.into(),
                    string.into(),
                ],
                "formatter.write",
            )
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Stage 5.6 Step 2: legacy `compile_drop_value`'s runtime releases (a
    /// dropped `Scheduler`, `Wait`, `Resolver`, or `CompletionToken`). Both
    /// emitters call it, so the call and its SSA shape are shared.
    pub(crate) fn build_runtime_release(
        &self,
        release: crate::RuntimeRelease,
        record: PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let name = match release {
            crate::RuntimeRelease::SchedulerDestroy => "__staple_sched_destroy",
            crate::RuntimeRelease::WaitDrop => "__staple_completion_wait_drop",
            crate::RuntimeRelease::ResolverDrop => "__staple_completion_resolver_drop",
            crate::RuntimeRelease::CompletionTokenRelease => "__staple_completion_token_release",
        };
        let pointer = self.context.ptr_type(AddressSpace::default());
        let function = self.declare_named_function(
            name,
            self.context.void_type().fn_type(&[pointer.into()], false),
        );
        self.builder
            .build_direct_call(function, &[record.into()], "")
            .map(|_| ())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// Stage 5.6 Step 2: dropping a `Coroutine` value calls the frame's
    /// idempotent cleanup function through the header slot. Shared so both
    /// emitters emit the same loads and indirect call.
    pub(crate) fn build_coroutine_frame_cleanup(
        &self,
        frame: PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let header_type = self.coroutine_header_type();
        let pointer = self.context.ptr_type(AddressSpace::default());
        let cleanup_slot = self
            .builder
            .build_struct_gep(
                header_type,
                frame,
                super::layout::CORO_CLEANUP_FN,
                "coro.cleanup.slot",
            )
            .map_err(compiler_diagnostic)?;
        let cleanup_ptr = self
            .builder
            .build_load(pointer, cleanup_slot, "coro.cleanup.fn")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let cleanup_type = self.context.void_type().fn_type(&[pointer.into()], false);
        self.builder
            .build_indirect_call(cleanup_type, cleanup_ptr, &[frame.into()], "")
            .map(|_| ())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// Stage 5.6 Step 2: legacy `compile_conditional_drop`'s branch, `drop`
    /// block, and continue block. Returns `(drop.live, drop.done)` and leaves
    /// the builder in the drop block with the live flag already cleared. The
    /// caller expands the glue, then calls [`Self::end_conditional_drop`].
    pub(crate) fn begin_conditional_drop(
        &self,
        live: PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<(BasicBlock<'context>, BasicBlock<'context>)> {
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span, "drop is not in a function"))?;
        let drop_block = self.context.append_basic_block(function, "drop.live");
        let done_block = self.context.append_basic_block(function, "drop.done");
        let condition = self
            .builder
            .build_load(self.context.bool_type(), live, "drop.is_live")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        self.builder
            .build_conditional_branch(condition, drop_block, done_block)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(drop_block);
        self.builder
            .build_store(live, self.context.bool_type().const_zero())
            .map_err(compiler_diagnostic)?;
        Ok((drop_block, done_block))
    }

    /// Stage 5.6 Step 2: close the skeleton [`Self::begin_conditional_drop`]
    /// opened.
    pub(crate) fn end_conditional_drop(
        &self,
        done_block: BasicBlock<'context>,
    ) -> CodeGenerationResult<()> {
        self.builder
            .build_unconditional_branch(done_block)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(done_block);
        Ok(())
    }

    /// Stage 5.6 Step 2: legacy `compile_conditional_cell_drop`'s state test
    /// and load. The caller expands the loaded value's glue, then calls
    /// [`Self::end_conditional_cell_drop`].
    pub(crate) fn begin_conditional_cell_drop(
        &self,
        cell: PointerValue<'context>,
        llvm_value_type: BasicTypeEnum<'context>,
        span: Span,
    ) -> CodeGenerationResult<CellDropBlocks<'context>> {
        let cell_type = self
            .context
            .struct_type(&[llvm_value_type, self.context.i8_type().into()], false);
        let state = self
            .builder
            .build_struct_gep(cell_type, cell, 1, "cell.drop.state")
            .map_err(compiler_diagnostic)?;
        let live = self
            .builder
            .build_load(self.context.i8_type(), state, "cell.drop.live")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let live = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                live,
                self.context.i8_type().const_int(2, false),
                "cell.drop.is_live",
            )
            .map_err(compiler_diagnostic)?;
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span, "cell drop outside a function"))?;
        let drop_block = self.context.append_basic_block(function, "cell.drop");
        let continue_block = self
            .context
            .append_basic_block(function, "cell.drop.continue");
        self.builder
            .build_conditional_branch(live, drop_block, continue_block)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(drop_block);
        let slot = self
            .builder
            .build_struct_gep(cell_type, cell, 0, "cell.drop.value")
            .map_err(compiler_diagnostic)?;
        let value = self
            .builder
            .build_load(llvm_value_type, slot, "cell.drop.loaded")
            .map_err(compiler_diagnostic)?;
        Ok(CellDropBlocks {
            value,
            state,
            continue_block,
        })
    }

    /// Stage 5.6 Step 2: clear the cell state and close the skeleton
    /// [`Self::begin_conditional_cell_drop`] opened.
    pub(crate) fn end_conditional_cell_drop(
        &self,
        blocks: &CellDropBlocks<'context>,
    ) -> CodeGenerationResult<()> {
        self.builder
            .build_store(blocks.state, self.context.i8_type().const_zero())
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(blocks.continue_block)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(blocks.continue_block);
        Ok(())
    }

    /// Stage 5.6 Step 2: one finalizer function `void(ptr)` declaration.
    pub(crate) fn add_finalizer_function(&self, name: &str) -> FunctionValue<'context> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        let function_type = self.context.void_type().fn_type(&[pointer.into()], false);
        self.llvm_module.add_function(name, function_type, None)
    }

    /// Stage 5.6 Step 2: legacy finalizer bodies open with an `entry` block,
    /// position the builder there, and take the payload pointer parameter.
    /// Restores the caller's position with
    /// [`Self::finish_finalizer_function`].
    pub(crate) fn enter_finalizer_function(
        &self,
        function: FunctionValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<FinalizerBody<'context>> {
        let previous_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);
        let payload = function
            .get_first_param()
            .ok_or_else(|| Diagnostic::new(span, "finalizer payload parameter is missing"))?
            .into_pointer_value();
        Ok(FinalizerBody {
            previous_block,
            payload,
        })
    }

    /// Stage 5.6 Step 2: emit the finalizer's `ret void` and restore the
    /// caller's insertion point.
    pub(crate) fn finish_finalizer_function(
        &self,
        body: &FinalizerBody<'context>,
    ) -> CodeGenerationResult<()> {
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = body.previous_block {
            self.builder.position_at_end(block);
        }
        Ok(())
    }

    /// Stage 5.6 Step 2: field 3 of a buffer header, the element data pointer.
    pub(crate) fn buffer_data_pointer(
        &self,
        buffer: PointerValue<'context>,
        element: BasicTypeEnum<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        self.builder
            .build_struct_gep(self.buffer_header_type(element), buffer, 3, "buffer.data")
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.6 Step 2: legacy `trap_if_buffer_frozen`.
    pub(crate) fn trap_if_buffer_frozen(
        &self,
        buffer: PointerValue<'context>,
        header: StructType<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let slot = self
            .builder
            .build_struct_gep(header, buffer, 2, "buffer.frozen.slot")
            .map_err(compiler_diagnostic)?;
        let frozen = self
            .builder
            .build_load(self.context.i8_type(), slot, "buffer.frozen")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let frozen = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                frozen,
                self.context.i8_type().const_zero(),
                "buffer.is_frozen",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(frozen, span)
    }

    /// Stage 5.6 Step 2: the capacity overflow trap `Buffer.with_capacity`
    /// performs before allocating its header.
    pub(crate) fn trap_if_buffer_capacity_overflows(
        &self,
        capacity: IntValue<'context>,
        llvm_element: BasicTypeEnum<'context>,
        header: StructType<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let offset = self
            .target_data
            .offset_of_element(&header, 3)
            .expect("Buffer data field has an offset");
        let stride = self.target_data.get_abi_size(&llvm_element);
        if stride == 0 {
            return Ok(());
        }
        let maximum = if self.size_type.get_bit_width() == 64 {
            u64::MAX
        } else {
            u32::MAX as u64
        };
        let maximum_capacity = (maximum - offset) / stride;
        let too_large = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGT,
                capacity,
                self.size_type.const_int(maximum_capacity, false),
                "buffer.capacity.overflow",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(too_large, span)
    }

    /// Stage 5.6 Step 2: the header allocation `Buffer.with_capacity` and
    /// `Buffer.clone` share. `prefix` is `"buffer"` or `"buffer.clone"` so the
    /// SSA names match each legacy site.
    pub(crate) fn build_buffer_allocation(
        &self,
        capacity: IntValue<'context>,
        llvm_element: BasicTypeEnum<'context>,
        header: StructType<'context>,
        prefix: &str,
        span: Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let offset = self
            .target_data
            .offset_of_element(&header, 3)
            .expect("Buffer data field has an offset");
        let stride = self.target_data.get_abi_size(&llvm_element);
        let bytes = self
            .builder
            .build_int_mul(
                capacity,
                self.size_type.const_int(stride, false),
                &format!("{prefix}.element.bytes"),
            )
            .map_err(compiler_diagnostic)?;
        let no_element_bytes = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                bytes,
                self.size_type.const_zero(),
                &format!("{prefix}.no.element.bytes"),
            )
            .map_err(compiler_diagnostic)?;
        let bytes = self
            .builder
            .build_select(
                no_element_bytes,
                self.size_type.const_int(1, false),
                bytes,
                &format!("{prefix}.physical.element.bytes"),
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let bytes = self
            .builder
            .build_int_add(
                bytes,
                self.size_type.const_int(offset, false),
                &format!("{prefix}.allocation.bytes"),
            )
            .map_err(compiler_diagnostic)?;
        let buffer =
            self.build_gc_allocation(bytes, &format!("{prefix}.allocate"), span.clone())?;
        self.builder
            .build_memset(
                buffer,
                self.target_data.get_abi_alignment(&header),
                self.context.i8_type().const_zero(),
                bytes,
            )
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(buffer)
    }

    /// Stage 5.6 Step 2: the address of one buffer element within an
    /// already-computed data pointer.
    pub(crate) fn build_buffer_element_pointer(
        &self,
        data: PointerValue<'context>,
        llvm_element: BasicTypeEnum<'context>,
        index: IntValue<'context>,
        name: &str,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        unsafe { self.builder.build_gep(llvm_element, data, &[index], name) }
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.6 Step 2: load one buffer element through its element pointer,
    /// returning the element's address too (a pop clears it after moving out).
    pub(crate) fn build_buffer_element_load(
        &self,
        data: PointerValue<'context>,
        llvm_element: BasicTypeEnum<'context>,
        index: IntValue<'context>,
        pointer_name: &str,
        load_name: &str,
    ) -> CodeGenerationResult<(PointerValue<'context>, BasicValueEnum<'context>)> {
        let slot = self.build_buffer_element_pointer(data, llvm_element, index, pointer_name)?;
        let value = self
            .builder
            .build_load(llvm_element, slot, load_name)
            .map_err(compiler_diagnostic)?;
        Ok((slot, value))
    }

    /// Stage 5.6 Step 2: store one buffer element through its element pointer.
    pub(crate) fn build_buffer_element_store(
        &self,
        data: PointerValue<'context>,
        llvm_element: BasicTypeEnum<'context>,
        index: IntValue<'context>,
        value: BasicValueEnum<'context>,
        pointer_name: &str,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let slot = self.build_buffer_element_pointer(data, llvm_element, index, pointer_name)?;
        self.builder
            .build_store(slot, value)
            .map(|_| ())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    /// Stage 5.6 Step 2: load a buffer header length field (field 0).
    pub(crate) fn build_buffer_length(
        &self,
        buffer: PointerValue<'context>,
        header: StructType<'context>,
        slot_name: &str,
        load_name: &str,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let slot = self
            .builder
            .build_struct_gep(header, buffer, 0, slot_name)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_load(self.size_type, slot, load_name)
            .map(|value| value.into_int_value())
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.6 Step 2: load a buffer header capacity field (field 1).
    pub(crate) fn build_buffer_capacity(
        &self,
        buffer: PointerValue<'context>,
        header: StructType<'context>,
        slot_name: &str,
        load_name: &str,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let slot = self
            .builder
            .build_struct_gep(header, buffer, 1, slot_name)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_load(self.size_type, slot, load_name)
            .map(|value| value.into_int_value())
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.6 Step 2: store a buffer header capacity field (field 1).
    pub(crate) fn build_buffer_capacity_store(
        &self,
        buffer: PointerValue<'context>,
        header: StructType<'context>,
        capacity: IntValue<'context>,
        slot_name: &str,
    ) -> CodeGenerationResult<()> {
        let slot = self
            .builder
            .build_struct_gep(header, buffer, 1, slot_name)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(slot, capacity)
            .map(|_| ())
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.5 Step 3: the type-independent phi core legacy uses for logical
    /// short-circuits and match merges.
    pub(crate) fn build_phi_value(
        &self,
        result_type: inkwell::types::BasicTypeEnum<'context>,
        incoming: &[(
            inkwell::values::BasicValueEnum<'context>,
            inkwell::basic_block::BasicBlock<'context>,
        )],
        name: &str,
    ) -> CodeGenerationResult<inkwell::values::BasicValueEnum<'context>> {
        let phi = self
            .builder
            .build_phi(result_type, name)
            .map_err(compiler_diagnostic)?;
        let incoming = incoming
            .iter()
            .map(|(value, block)| (value as &dyn inkwell::values::BasicValue<'context>, *block))
            .collect::<Vec<_>>();
        phi.add_incoming(&incoming);
        Ok(phi.as_basic_value())
    }
}

/// Stage 5.5 Step 3: a sum-coercion storage slot and its tag/payload views.
pub(crate) struct SumStorageSlot<'context> {
    pub llvm_type: inkwell::types::StructType<'context>,
    pub slot: inkwell::values::PointerValue<'context>,
    pub storage: super::layout::SumStorage<'context>,
}

/// Stage 5.6 Step 2: the state [`Backend::begin_conditional_cell_drop`] opened:
/// the loaded cell value, its state byte, and the merge block.
pub(crate) struct CellDropBlocks<'context> {
    pub value: BasicValueEnum<'context>,
    pub state: PointerValue<'context>,
    pub continue_block: BasicBlock<'context>,
}

/// Stage 5.6 Step 2: the state [`Backend::enter_finalizer_function`] opened.
pub(crate) struct FinalizerBody<'context> {
    pub previous_block: Option<BasicBlock<'context>>,
    pub payload: PointerValue<'context>,
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
