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
