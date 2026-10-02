//! the backend-local runtime layer.
//!
//! Installs the hand-written `.ll` runtime modules, declares the lazily
//! referenced libc/runtime helpers, and emits the fixed UTF-8 validator. These helpers depend only on LLVM and recorded layouts.

use inkwell::{AddressSpace, memory_buffer::MemoryBuffer};

use staple_syntax::Span;

use super::{Backend, CodeGenerationResult, Diagnostic, compiler_diagnostic};

impl<'program, 'context> Backend<'program, 'context> {
    /// Emits a direct runtime call from an already-selected LLVM declaration.
    pub(crate) fn build_runtime_call(
        &self,
        function: inkwell::values::FunctionValue<'context>,
        arguments: &[inkwell::values::BasicMetadataValueEnum<'context>],
        call_name: &str,
    ) -> CodeGenerationResult<inkwell::values::CallSiteValue<'context>> {
        self.builder
            .build_direct_call(function, arguments, call_name)
            .map_err(compiler_diagnostic)
    }

    pub(crate) fn build_reactive_runtime_call(
        &self,
        name: &str,
        arguments: &[inkwell::values::BasicMetadataValueEnum<'context>],
        _result: Option<inkwell::types::BasicTypeEnum<'context>>,
        call_name: &str,
        span: Span,
    ) -> CodeGenerationResult<Option<inkwell::values::BasicValueEnum<'context>>> {
        let function = self.llvm_module.get_function(name).ok_or_else(|| {
            Diagnostic::new(
                span.clone(),
                format!("missing reactive runtime function `{name}`"),
            )
        })?;
        let call = self
            .builder
            .build_direct_call(function, arguments, call_name)
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(call.try_as_basic_value().basic())
    }

    pub(crate) fn install_gc_runtime(&self) -> CodeGenerationResult<()> {
        let pointer_bytes = self.target_data.get_pointer_byte_size(None) as u64;
        let pointer_shift = pointer_bytes.trailing_zeros();
        let bits = pointer_bytes * 8;
        let size = format!("i{bits}");
        let maximum = if bits == 64 {
            u64::MAX.to_string()
        } else {
            ((1_u64 << bits) - 1).to_string()
        };
        let maximum_half = if bits == 64 {
            (u64::MAX / 2).to_string()
        } else {
            (((1_u64 << bits) - 1) / 2).to_string()
        };
        let maximum_allocation = if bits == 64 {
            (u64::MAX - pointer_bytes * 5).to_string()
        } else {
            (((1_u64 << bits) - 1) - pointer_bytes * 5).to_string()
        };
        let runtime = include_str!("../gc.ll")
            .replace("{{SIZE}}", &size)
            .replace("{{PTR_BYTES}}", &pointer_bytes.to_string())
            .replace("{{PTR_SHIFT}}", &pointer_shift.to_string())
            .replace("{{HEADER_BYTES}}", &(pointer_bytes * 5).to_string())
            .replace("{{ROOT_BYTES}}", &(pointer_bytes * 3).to_string())
            .replace("{{REGISTER_BYTES}}", &(pointer_bytes * 64).to_string())
            .replace("{{MAX_HALF}}", &maximum_half)
            .replace("{{MAX_ALLOC}}", &maximum_allocation)
            .replace("{{MAX}}", &maximum);
        let buffer = MemoryBuffer::create_from_memory_range_copy(runtime.as_bytes(), "staple-gc");
        let module = self
            .context
            .create_module_from_ir(buffer)
            .map_err(|error| {
                Diagnostic::new(
                    Span::Compiler,
                    format!("could not build garbage collector runtime: {error}"),
                )
            })?;
        self.llvm_module.link_in_module(module).map_err(|error| {
            Diagnostic::new(
                Span::Compiler,
                format!("could not link garbage collector runtime: {error}"),
            )
        })
    }

    pub(crate) fn install_coroutine_runtime(&self) -> CodeGenerationResult<()> {
        let pointer_bytes = self.target_data.get_pointer_byte_size(None) as u64;
        let bits = pointer_bytes * 8;
        let runtime = include_str!("../coroutine.ll").replace("{{SIZE}}", &format!("i{bits}"));
        let buffer =
            MemoryBuffer::create_from_memory_range_copy(runtime.as_bytes(), "staple-coroutine");
        let module = self
            .context
            .create_module_from_ir(buffer)
            .map_err(|error| {
                Diagnostic::new(
                    Span::Compiler,
                    format!("could not build coroutine runtime: {error}"),
                )
            })?;
        self.llvm_module.link_in_module(module).map_err(|error| {
            Diagnostic::new(
                Span::Compiler,
                format!("could not link coroutine runtime: {error}"),
            )
        })
    }

    pub(crate) fn install_reactive_runtime(&self) -> CodeGenerationResult<()> {
        let pointer_bytes = self.target_data.get_pointer_byte_size(None) as u64;
        let bits = pointer_bytes * 8;
        let runtime = include_str!("../reactive.ll")
            .replace("{{SIZE}}", &format!("i{bits}"))
            .replace("{{SCOPE_BYTES}}", &pointer_bytes.to_string())
            .replace("{{SIGNAL_BYTES}}", &pointer_bytes.to_string())
            .replace("{{REACTION_BYTES}}", &(pointer_bytes * 8).to_string())
            .replace("{{DEP_BYTES}}", &(pointer_bytes * 5).to_string())
            .replace("{{WORK_BYTES}}", &(pointer_bytes * 3).to_string());
        let buffer =
            MemoryBuffer::create_from_memory_range_copy(runtime.as_bytes(), "staple-reactive");
        let module = self
            .context
            .create_module_from_ir(buffer)
            .map_err(|error| {
                Diagnostic::new(
                    Span::Compiler,
                    format!("could not build reactive runtime: {error}"),
                )
            })?;
        self.llvm_module.link_in_module(module).map_err(|error| {
            Diagnostic::new(
                Span::Compiler,
                format!("could not link reactive runtime: {error}"),
            )
        })
    }

    /// Declares (once) an external symbol the emitted code calls by name: a
    /// runtime-module helper (for example `__staple_sched_create`) or a libc
    /// function (`free`, `memcmp`, `snprintf`, `strlen`, `memchr`). The runtime
    /// modules are linked first, so a declaration that already exists wins.
    pub(crate) fn declare_named_function(
        &self,
        name: &str,
        signature: inkwell::types::FunctionType<'context>,
    ) -> inkwell::values::FunctionValue<'context> {
        self.llvm_module
            .get_function(name)
            .unwrap_or_else(|| self.llvm_module.add_function(name, signature, None))
    }

    /// Emits the fixed `__staple_is_valid_utf8` validator and returns it, so the
    /// caller can record its runtime requirement in tests.
    pub(crate) fn build_utf8_validator(
        &mut self,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let function_type = self
            .context
            .bool_type()
            .fn_type(&[pointer_type.into(), self.size_type.into()], false);
        let function = self.llvm_module.add_function(
            "__staple_is_valid_utf8",
            function_type,
            Some(inkwell::module::Linkage::Internal),
        );
        let entry = self.context.append_basic_block(function, "entry");
        let loop_block = self.context.append_basic_block(function, "loop");
        let byte_block = self.context.append_basic_block(function, "byte");
        let done_block = self.context.append_basic_block(function, "done");
        let continuation_block = self.context.append_basic_block(function, "continuation");
        let continuation_valid = self
            .context
            .append_basic_block(function, "continuation.valid");
        let leading_block = self.context.append_basic_block(function, "leading");
        let leading_valid = self.context.append_basic_block(function, "leading.valid");
        let invalid_block = self.context.append_basic_block(function, "invalid");

        let pointer = function
            .get_nth_param(0)
            .ok_or_else(|| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "internal invariant violated: UTF-8 validator has a pointer parameter",
                )
            })?
            .into_pointer_value();
        let length = function
            .get_nth_param(1)
            .ok_or_else(|| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "internal invariant violated: UTF-8 validator has a length parameter",
                )
            })?
            .into_int_value();
        let byte_type = self.context.i8_type();
        self.builder.position_at_end(entry);
        let index_slot = self
            .builder
            .build_alloca(self.size_type, "index")
            .map_err(compiler_diagnostic)?;
        let remaining_slot = self
            .builder
            .build_alloca(byte_type, "remaining")
            .map_err(compiler_diagnostic)?;
        let minimum_slot = self
            .builder
            .build_alloca(byte_type, "minimum")
            .map_err(compiler_diagnostic)?;
        let maximum_slot = self
            .builder
            .build_alloca(byte_type, "maximum")
            .map_err(compiler_diagnostic)?;
        for (slot, value) in [
            (index_slot, self.size_type.const_zero()),
            (remaining_slot, byte_type.const_zero()),
            (minimum_slot, byte_type.const_int(0x80, false)),
            (maximum_slot, byte_type.const_int(0xbf, false)),
        ] {
            self.builder
                .build_store(slot, value)
                .map_err(compiler_diagnostic)?;
        }
        self.builder
            .build_unconditional_branch(loop_block)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(loop_block);
        let index = self
            .builder
            .build_load(self.size_type, index_slot, "index")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let at_end = self
            .builder
            .build_int_compare(inkwell::IntPredicate::EQ, index, length, "at_end")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(at_end, done_block, byte_block)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(done_block);
        let remaining = self
            .builder
            .build_load(byte_type, remaining_slot, "remaining")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let complete = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                remaining,
                byte_type.const_zero(),
                "complete",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_return(Some(&complete))
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(byte_block);
        let byte_pointer = unsafe {
            self.builder
                .build_gep(byte_type, pointer, &[index], "byte.pointer")
        }
        .map_err(compiler_diagnostic)?;
        let byte = self
            .builder
            .build_load(byte_type, byte_pointer, "byte")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let remaining = self
            .builder
            .build_load(byte_type, remaining_slot, "remaining")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let expects_continuation = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                remaining,
                byte_type.const_zero(),
                "expects_continuation",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(expects_continuation, continuation_block, leading_block)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(continuation_block);
        let minimum = self
            .builder
            .build_load(byte_type, minimum_slot, "minimum")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let maximum = self
            .builder
            .build_load(byte_type, maximum_slot, "maximum")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let above_minimum = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGE, byte, minimum, "above_minimum")
            .map_err(compiler_diagnostic)?;
        let below_maximum = self
            .builder
            .build_int_compare(inkwell::IntPredicate::ULE, byte, maximum, "below_maximum")
            .map_err(compiler_diagnostic)?;
        let valid_continuation = self
            .builder
            .build_and(above_minimum, below_maximum, "valid_continuation")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(valid_continuation, continuation_valid, invalid_block)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(continuation_valid);
        let next_remaining = self
            .builder
            .build_int_sub(remaining, byte_type.const_int(1, false), "next_remaining")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(remaining_slot, next_remaining)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(minimum_slot, byte_type.const_int(0x80, false))
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(maximum_slot, byte_type.const_int(0xbf, false))
            .map_err(compiler_diagnostic)?;
        self.increment_utf8_index(index_slot, index, loop_block)?;

        self.builder.position_at_end(leading_block);
        let ascii = self.byte_in_range(byte, 0, 0x7f)?;
        let two = self.byte_in_range(byte, 0xc2, 0xdf)?;
        let three_low = self.byte_in_range(byte, 0xe1, 0xec)?;
        let three_high = self.byte_in_range(byte, 0xee, 0xef)?;
        let three_general = self
            .builder
            .build_or(three_low, three_high, "three.general")
            .map_err(compiler_diagnostic)?;
        let e0 = self.byte_equals(byte, 0xe0)?;
        let ed = self.byte_equals(byte, 0xed)?;
        let three = self
            .builder
            .build_or(e0, ed, "three.special")
            .and_then(|special| self.builder.build_or(special, three_general, "three"))
            .map_err(compiler_diagnostic)?;
        let four_general = self.byte_in_range(byte, 0xf1, 0xf3)?;
        let f0 = self.byte_equals(byte, 0xf0)?;
        let f4 = self.byte_equals(byte, 0xf4)?;
        let four = self
            .builder
            .build_or(f0, f4, "four.special")
            .and_then(|special| self.builder.build_or(special, four_general, "four"))
            .map_err(compiler_diagnostic)?;
        let valid_leading = self
            .builder
            .build_or(ascii, two, "leading.short")
            .and_then(|short| self.builder.build_or(short, three, "leading.three"))
            .and_then(|partial| self.builder.build_or(partial, four, "valid_leading"))
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(valid_leading, leading_valid, invalid_block)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(leading_valid);
        let three_or_four = self
            .builder
            .build_or(three, four, "three_or_four")
            .map_err(compiler_diagnostic)?;
        let remaining_for_multibyte = self
            .builder
            .build_select(
                four,
                byte_type.const_int(3, false),
                byte_type.const_int(2, false),
                "long_remaining",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let remaining = self
            .builder
            .build_select(
                two,
                byte_type.const_int(1, false),
                remaining_for_multibyte,
                "multibyte_remaining",
            )
            .and_then(|value| {
                self.builder.build_select(
                    ascii,
                    byte_type.const_zero(),
                    value.into_int_value(),
                    "remaining",
                )
            })
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let minimum = self
            .builder
            .build_select(
                e0,
                byte_type.const_int(0xa0, false),
                byte_type.const_int(0x80, false),
                "minimum.e0",
            )
            .and_then(|value| {
                self.builder.build_select(
                    f0,
                    byte_type.const_int(0x90, false),
                    value.into_int_value(),
                    "minimum",
                )
            })
            .map_err(compiler_diagnostic)?;
        let maximum = self
            .builder
            .build_select(
                ed,
                byte_type.const_int(0x9f, false),
                byte_type.const_int(0xbf, false),
                "maximum.ed",
            )
            .and_then(|value| {
                self.builder.build_select(
                    f4,
                    byte_type.const_int(0x8f, false),
                    value.into_int_value(),
                    "maximum",
                )
            })
            .map_err(compiler_diagnostic)?;
        let _ = three_or_four;
        self.builder
            .build_store(remaining_slot, remaining)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(minimum_slot, minimum)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(maximum_slot, maximum)
            .map_err(compiler_diagnostic)?;
        self.increment_utf8_index(index_slot, index, loop_block)?;

        self.builder.position_at_end(invalid_block);
        self.builder
            .build_return(Some(&self.context.bool_type().const_zero()))
            .map_err(compiler_diagnostic)?;
        Ok(function)
    }
}
