//! Explicit numeric/address conversions and checked string parsing.
//!
//! Checked numeric operations sanitize float operands before LLVM fptosi/fptoui:
//! an invalid input must produce an error, never poison or undefined execution.

use super::{Backend, CodeGenerationResult, Span, compiler_diagnostic};
use crate::{FloatType, IntegerType, NumericType};
use inkwell::{
    AddressSpace, FloatPredicate, IntPredicate,
    values::{BasicValueEnum, FloatValue, IntValue, StructValue},
};

impl<'program, 'context> Backend<'program, 'context> {
    fn conversion_result(
        &self,
        status: IntValue<'context>,
        value: BasicValueEnum<'context>,
    ) -> CodeGenerationResult<StructValue<'context>> {
        let ty = self
            .context
            .struct_type(&[self.context.i8_type().into(), value.get_type()], false);
        let result = self
            .builder
            .build_insert_value(ty.get_undef(), status, 0, "conversion.status")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(result, value, 1, "conversion.value")
            .map_err(compiler_diagnostic)?
            .into_struct_value())
    }

    fn conversion_status(
        &self,
        valid: IntValue<'context>,
        error: u64,
    ) -> CodeGenerationResult<IntValue<'context>> {
        Ok(self
            .builder
            .build_select(
                valid,
                self.context.i8_type().const_zero(),
                self.context.i8_type().const_int(error, false),
                "conversion.status",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value())
    }

    fn float_in_integer_range(
        &self,
        value: FloatValue<'context>,
        integer: IntegerType,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let bits = self.compile_integer_type(integer).get_bit_width();
        let ty = value.get_type();
        let (minimum, exclusive_maximum) = if integer.is_signed() {
            (
                -2.0_f64.powi(bits as i32 - 1),
                2.0_f64.powi(bits as i32 - 1),
            )
        } else {
            (0.0, 2.0_f64.powi(bits as i32))
        };
        let lower = self
            .builder
            .build_float_compare(
                FloatPredicate::OGE,
                value,
                ty.const_float(minimum),
                "conversion.lower_bound",
            )
            .map_err(compiler_diagnostic)?;
        let upper = self
            .builder
            .build_float_compare(
                FloatPredicate::OLT,
                value,
                ty.const_float(exclusive_maximum),
                "conversion.upper_bound",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_and(lower, upper, "conversion.in_range")
            .map_err(compiler_diagnostic)
    }

    fn float_is_finite(
        &self,
        value: FloatValue<'context>,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let ty = value.get_type();
        let lower = self
            .builder
            .build_float_compare(
                FloatPredicate::OGT,
                value,
                ty.const_float(f64::NEG_INFINITY),
                "conversion.finite.lower",
            )
            .map_err(compiler_diagnostic)?;
        let upper = self
            .builder
            .build_float_compare(
                FloatPredicate::OLT,
                value,
                ty.const_float(f64::INFINITY),
                "conversion.finite.upper",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_and(lower, upper, "conversion.finite")
            .map_err(compiler_diagnostic)
    }

    fn float_to_integer(
        &self,
        value: FloatValue<'context>,
        to: IntegerType,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let ty = self.compile_integer_type(to);
        if to.is_signed() {
            self.builder
                .build_float_to_signed_int(value, ty, "conversion.integer")
        } else {
            self.builder
                .build_float_to_unsigned_int(value, ty, "conversion.integer")
        }
        .map_err(compiler_diagnostic)
    }

    fn integer_to_float(
        &self,
        value: IntValue<'context>,
        from: IntegerType,
        to: FloatType,
    ) -> CodeGenerationResult<FloatValue<'context>> {
        let ty = self.compile_float_type(to);
        if from.is_signed() {
            self.builder
                .build_signed_int_to_float(value, ty, "conversion.float")
        } else {
            self.builder
                .build_unsigned_int_to_float(value, ty, "conversion.float")
        }
        .map_err(compiler_diagnostic)
    }

    pub(crate) fn build_numeric_conversion(
        &self,
        from: NumericType,
        to: NumericType,
        value: BasicValueEnum<'context>,
    ) -> CodeGenerationResult<StructValue<'context>> {
        let zero = self.context.i8_type().const_zero();
        let (status, converted): (_, BasicValueEnum<'context>) = match (from, to) {
            (NumericType::Integer(from), NumericType::Integer(to)) => {
                let value = value.into_int_value();
                let wide = self.context.i128_type();
                let extended = self
                    .builder
                    .build_int_cast_sign_flag(value, wide, from.is_signed(), "conversion.wide")
                    .map_err(compiler_diagnostic)?;
                let target = self.compile_integer_type(to);
                let bits = target.get_bit_width();
                let minimum = if to.is_signed() {
                    -(1_i128 << (bits - 1))
                } else {
                    0
                };
                let maximum = if to.is_signed() {
                    (1_u64 << (bits - 1)) - 1
                } else if bits == 64 {
                    u64::MAX
                } else {
                    (1_u64 << bits) - 1
                };
                let lower = self
                    .builder
                    .build_int_compare(
                        IntPredicate::SGE,
                        extended,
                        wide.const_int(minimum as u64, true),
                        "conversion.lower_bound",
                    )
                    .map_err(compiler_diagnostic)?;
                let upper = self
                    .builder
                    .build_int_compare(
                        IntPredicate::SLE,
                        extended,
                        wide.const_int(maximum, false),
                        "conversion.upper_bound",
                    )
                    .map_err(compiler_diagnostic)?;
                let valid = self
                    .builder
                    .build_and(lower, upper, "conversion.in_range")
                    .map_err(compiler_diagnostic)?;
                let result = self
                    .builder
                    .build_int_cast_sign_flag(value, target, from.is_signed(), "conversion.integer")
                    .map_err(compiler_diagnostic)?;
                (self.conversion_status(valid, 1)?, result.into())
            }
            (NumericType::Integer(from), NumericType::Float(to)) => {
                let value = value.into_int_value();
                let converted = self.integer_to_float(value, from, to)?;
                let in_range = self.float_in_integer_range(converted, from)?;
                let sanitized = self
                    .builder
                    .build_select(
                        in_range,
                        converted,
                        converted.get_type().const_zero(),
                        "conversion.safe_float",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_float_value();
                let restored = self.float_to_integer(sanitized, from)?;
                let equal = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, restored, value, "conversion.exact")
                    .map_err(compiler_diagnostic)?;
                let exact = self
                    .builder
                    .build_and(in_range, equal, "conversion.valid")
                    .map_err(compiler_diagnostic)?;
                (self.conversion_status(exact, 2)?, converted.into())
            }
            (NumericType::Float(from), NumericType::Integer(to)) => {
                let value = value.into_float_value();
                let in_range = self.float_in_integer_range(value, to)?;
                let sanitized = self
                    .builder
                    .build_select(
                        in_range,
                        value,
                        value.get_type().const_zero(),
                        "conversion.safe_float",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_float_value();
                let converted = self.float_to_integer(sanitized, to)?;
                let restored = self.integer_to_float(converted, to, from)?;
                let exact = self
                    .builder
                    .build_float_compare(FloatPredicate::OEQ, value, restored, "conversion.exact")
                    .map_err(compiler_diagnostic)?;
                let precision = self.conversion_status(exact, 2)?;
                let ranged = self
                    .builder
                    .build_select(
                        in_range,
                        precision,
                        self.context.i8_type().const_int(1, false),
                        "conversion.range_status",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let finite = self.float_is_finite(value)?;
                let status = self
                    .builder
                    .build_select(
                        finite,
                        ranged,
                        self.context.i8_type().const_int(3, false),
                        "conversion.finite_status",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                (status, converted.into())
            }
            (NumericType::Float(from), NumericType::Float(to)) => {
                let value = value.into_float_value();
                let converted = self
                    .builder
                    .build_float_cast(value, self.compile_float_type(to), "conversion.float")
                    .map_err(compiler_diagnostic)?;
                if from == FloatType::F32 || from == to {
                    (zero, converted.into())
                } else {
                    let restored = self
                        .builder
                        .build_float_cast(converted, value.get_type(), "conversion.restored")
                        .map_err(compiler_diagnostic)?;
                    // NaNs and infinities are representable; NaN payload preservation is not promised.
                    let exact = self
                        .builder
                        .build_float_compare(
                            FloatPredicate::UEQ,
                            value,
                            restored,
                            "conversion.exact",
                        )
                        .map_err(compiler_diagnostic)?;
                    let finite_input = self.float_is_finite(value)?;
                    let finite_output = self.float_is_finite(converted)?;
                    let overflow = self
                        .builder
                        .build_and(
                            finite_input,
                            self.builder
                                .build_not(finite_output, "conversion.output_nonfinite")
                                .map_err(compiler_diagnostic)?,
                            "conversion.overflow",
                        )
                        .map_err(compiler_diagnostic)?;
                    let precision = self.conversion_status(exact, 2)?;
                    let status = self
                        .builder
                        .build_select(
                            overflow,
                            self.context.i8_type().const_int(1, false),
                            precision,
                            "conversion.status",
                        )
                        .map_err(compiler_diagnostic)?
                        .into_int_value();
                    (status, converted.into())
                }
            }
        };
        self.conversion_result(status, converted)
    }

    pub(crate) fn build_validate_utf8(
        &self,
        bytes: StructValue<'context>,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let pointer = self
            .builder
            .build_extract_value(bytes, 0, "utf8.pointer")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_extract_value(bytes, 1, "utf8.length")
            .map_err(compiler_diagnostic)?;
        let validator = self
            .llvm_module
            .get_function("__staple_is_valid_utf8")
            .expect("UTF-8 validation is recorded before emission");
        let valid = self
            .builder
            .build_direct_call(validator, &[pointer.into(), length.into()], "utf8.valid")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_int_z_extend(valid, self.context.i8_type(), "utf8.status")
            .map_err(compiler_diagnostic)
    }

    pub(crate) fn build_string_has_nul(
        &self,
        string: StructValue<'context>,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let pointer = self
            .builder
            .build_extract_value(string, 0, "string.pointer")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_extract_value(string, 1, "string.length")
            .map_err(compiler_diagnostic)?;
        let memchr = self
            .llvm_module
            .get_function("memchr")
            .expect("interior-NUL check is recorded");
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
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let has_nul = self
            .builder
            .build_is_not_null(nul, "string.has_interior_nul")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_int_z_extend(has_nul, self.context.i8_type(), "string.nul_status")
            .map_err(compiler_diagnostic)
    }

    pub(crate) fn build_c_string_bytes(
        &self,
        pointer: inkwell::values::PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<StructValue<'context>> {
        let strlen = self
            .llvm_module
            .get_function("strlen")
            .expect("CString length is recorded");
        let length = self
            .builder
            .build_direct_call(strlen, &[pointer.into()], "c_string.length")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.build_string_value(pointer, length, span)
    }

    fn errno_function_name(&self) -> &'static str {
        let triple = self.llvm_module.get_triple();
        let triple = triple.as_str().to_string_lossy();
        if triple.contains("apple") || triple.contains("freebsd") {
            "__error"
        } else if triple.contains("windows") {
            "_errno"
        } else {
            "__errno_location"
        }
    }

    pub(crate) fn declare_numeric_parsers(&self) {
        let pointer = self.context.ptr_type(AddressSpace::default());
        let integer = self.context.i64_type().fn_type(
            &[
                pointer.into(),
                pointer.into(),
                self.context.i32_type().into(),
            ],
            false,
        );
        self.declare_named_function("strtoll", integer);
        self.declare_named_function("strtoull", integer);
        self.declare_named_function(
            "strtof",
            self.context
                .f32_type()
                .fn_type(&[pointer.into(), pointer.into()], false),
        );
        self.declare_named_function(
            "strtod",
            self.context
                .f64_type()
                .fn_type(&[pointer.into(), pointer.into()], false),
        );
        self.declare_named_function(self.errno_function_name(), pointer.fn_type(&[], false));
    }

    pub(crate) fn build_parse_number(
        &self,
        to: NumericType,
        string: StructValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<StructValue<'context>> {
        let pointer = self
            .builder
            .build_extract_value(string, 0, "parse.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let length = self
            .builder
            .build_extract_value(string, 1, "parse.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let overflow = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                length,
                self.size_type.const_all_ones(),
                "parse.length_overflow",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(overflow, span.clone())?;
        let allocation = self
            .builder
            .build_int_add(length, self.size_type.const_int(1, false), "parse.capacity")
            .map_err(compiler_diagnostic)?;
        let buffer = self.build_gc_allocation(allocation, "parse.buffer", span)?;
        self.builder
            .build_memcpy(buffer, 1, pointer, 1, length)
            .map_err(compiler_diagnostic)?;
        let end = unsafe {
            self.builder
                .build_gep(self.context.i8_type(), buffer, &[length], "parse.end")
        }
        .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(end, self.context.i8_type().const_zero())
            .map_err(compiler_diagnostic)?;
        let end_slot = self
            .entry_builder()
            .build_alloca(
                self.context.ptr_type(AddressSpace::default()),
                "parse.end_slot",
            )
            .map_err(compiler_diagnostic)?;
        let errno_function = self
            .llvm_module
            .get_function(self.errno_function_name())
            .expect("parser errno is recorded");
        let errno_pointer = self
            .builder
            .build_direct_call(errno_function, &[], "parse.errno_pointer")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let saved_errno = self
            .builder
            .build_load(self.context.i32_type(), errno_pointer, "parse.saved_errno")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(errno_pointer, self.context.i32_type().const_zero())
            .map_err(compiler_diagnostic)?;
        let (name, from) = match to {
            NumericType::Integer(integer) if integer.is_signed() => {
                ("strtoll", NumericType::Integer(IntegerType::I64))
            }
            NumericType::Integer(_) => ("strtoull", NumericType::Integer(IntegerType::U64)),
            NumericType::Float(FloatType::F32) => ("strtof", to),
            NumericType::Float(FloatType::F64) => ("strtod", to),
        };
        let parser = self
            .llvm_module
            .get_function(name)
            .expect("numeric parser is recorded");
        let mut arguments = vec![buffer.into(), end_slot.into()];
        if matches!(to, NumericType::Integer(_)) {
            arguments.push(self.context.i32_type().const_int(10, false).into());
        }
        let parsed = self
            .builder
            .build_direct_call(parser, &arguments, "parse.value")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic();
        let errno = self
            .builder
            .build_load(self.context.i32_type(), errno_pointer, "parse.errno")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        self.builder
            .build_store(errno_pointer, saved_errno)
            .map_err(compiler_diagnostic)?;
        let parsed_end = self
            .builder
            .build_load(
                self.context.ptr_type(AddressSpace::default()),
                end_slot,
                "parse.parsed_end",
            )
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let complete = self
            .builder
            .build_int_compare(IntPredicate::EQ, parsed_end, end, "parse.complete")
            .map_err(compiler_diagnostic)?;
        let nonempty = self
            .builder
            .build_int_compare(IntPredicate::NE, parsed_end, buffer, "parse.nonempty")
            .map_err(compiler_diagnostic)?;
        let first = self
            .builder
            .build_load(self.context.i8_type(), buffer, "parse.first")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let no_whitespace = self
            .builder
            .build_int_compare(
                IntPredicate::UGT,
                first,
                self.context.i8_type().const_int(32, false),
                "parse.no_whitespace",
            )
            .map_err(compiler_diagnostic)?;
        let mut syntax = self
            .builder
            .build_and(complete, nonempty, "parse.syntax")
            .map_err(compiler_diagnostic)?;
        syntax = self
            .builder
            .build_and(syntax, no_whitespace, "parse.syntax")
            .map_err(compiler_diagnostic)?;
        if matches!(to, NumericType::Integer(integer) if !integer.is_signed()) {
            let nonnegative = self
                .builder
                .build_int_compare(
                    IntPredicate::NE,
                    first,
                    self.context.i8_type().const_int(b'-' as u64, false),
                    "parse.nonnegative",
                )
                .map_err(compiler_diagnostic)?;
            syntax = self
                .builder
                .build_and(syntax, nonnegative, "parse.syntax")
                .map_err(compiler_diagnostic)?;
        }
        let libc_range = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                errno,
                self.context.i32_type().const_zero(),
                "parse.in_range",
            )
            .map_err(compiler_diagnostic)?;
        let result = self.build_numeric_conversion(from, to, parsed)?;
        let conversion_status = self
            .builder
            .build_extract_value(result, 0, "parse.conversion_status")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let range_status = self
            .builder
            .build_select(
                libc_range,
                conversion_status,
                self.context.i8_type().const_int(1, false),
                "parse.range_status",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let status = self
            .builder
            .build_select(
                syntax,
                range_status,
                self.context.i8_type().const_int(4, false),
                "parse.status",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let value = self
            .builder
            .build_extract_value(result, 1, "parse.converted")
            .map_err(compiler_diagnostic)?;
        self.conversion_result(status, value)
    }
}
