//! The backend-local LLVM layout layer.
//!
//! Everything here decides the machine representation of a concrete Staple
//! type without consulting the checker: integer, float, product, sum, closure,
//! slice, buffer-header, coroutine-frame, task-record, and completion-record
//! layouts, plus the fixed field indices of the runtime records these types
//! describe. The emitter reads only recorded lowering decisions.
//!
//! [`LayoutContext`] is built from the lowered program's semantic IDs
//! (the type catalog) and `runtime_opaque_kind`, so layout decisions have one
//! source: the lowering records, never the checker.

use inkwell::{AddressSpace, types::BasicType, types::BasicTypeEnum};

use crate::lower::EmissionView;
use crate::{CheckedProductType, CheckedType, FloatType, IntegerType, TypeId};

use super::{Backend, CodeGenerationResult, Diagnostic, Span};

/// Fixed coroutine frame header field indices (see `coroutine.ll`).
pub(crate) const CORO_STATE: u32 = 0;
pub(crate) const CORO_RESUME_FN: u32 = 1;
pub(crate) const CORO_CLEANUP_FN: u32 = 2;
pub(crate) const CORO_CAPTURE_ENV: u32 = 3;
pub(crate) const CORO_CHILD: u32 = 4;
pub(crate) const CORO_RESULT_PTR: u32 = 5;
pub(crate) const CORO_PENDING_PTR: u32 = 6;
#[allow(dead_code)] // read only by `coroutine.ll`'s driver
pub(crate) const CORO_PARENT: u32 = 7;
/// Pointer to a GC-allocated bundle of the coroutine's deferred-effect resource
/// values, packed by whoever drives the frame and unpacked by `resume`.
pub(crate) const CORO_RESOURCES: u32 = 8;
/// Pointer to this task's `%TaskRecord` when it was `spawn`ed (else null); the
/// driver marks it complete when the root frame finishes.
pub(crate) const CORO_RECORD: u32 = 9;
pub(crate) const CORO_HEADER_FIELDS: u32 = 10;

/// `frame->state`: `0..=resume_points` are live resume states (`0` also means
/// "created, never resumed"); these two markers are terminal.
pub(crate) const CORO_STATE_DONE: u64 = 254;
pub(crate) const CORO_STATE_FREED: u64 = 255;
/// `%CoroStatus` status codes returned by `resume` (see `coroutine.ll`).
pub(crate) const CORO_STATUS_DONE: u64 = 0;
pub(crate) const CORO_STATUS_RESUME_CHILD: u64 = 1;
/// The body unwound after a cancellation request; the frame is spent.
pub(crate) const CORO_STATUS_CANCELLED: u64 = 3;
/// The body is `await`-ing a spawned `Task`; it has registered itself as that
/// task's waiter and parks until the task completes.
pub(crate) const CORO_STATUS_WAIT_TASK: u64 = 4;
/// The body is `await`-ing an external `Wait` / `Task`: it has registered a
/// waiter and parks until woken. Driver behaviour is identical to `WAIT_TASK`;
/// the two names distinguish the record kind at the lowering site.
pub(crate) const CORO_STATUS_WAIT_EXTERNAL: u64 = CORO_STATUS_WAIT_TASK;

/// `%Completion` field indices (see `coroutine.ll`). The record is
/// `{ i8 state, i8 flags, {{SIZE}} generation, ptr scheduler, ptr waiter,
/// ptr cancel_env, ptr cancel_fn, T value }`; the runtime only touches the
/// header. Cancel callbacks are null until armed.
#[allow(dead_code)] // field 0; loaded directly through the record pointer
pub(crate) const COMPLETION_STATE: u32 = 0;
pub(crate) const COMPLETION_FLAGS: u32 = 1;
#[allow(dead_code)] // bumped only by `coroutine.ll`
pub(crate) const COMPLETION_GENERATION: u32 = 2;
pub(crate) const COMPLETION_SCHEDULER: u32 = 3;
#[allow(dead_code)] // written only by `coroutine.ll`
pub(crate) const COMPLETION_WAITER: u32 = 4;
pub(crate) const COMPLETION_CANCEL_ENV: u32 = 5;
pub(crate) const COMPLETION_CANCEL_FN: u32 = 6;
pub(crate) const COMPLETION_VALUE: u32 = 7;
/// `%Completion.state`: 0 pending, 1 completed, 2 cancelled, 3 consumer-gone.
pub(crate) const COMPLETION_STATE_COMPLETED: u64 = 1;
/// `%Completion.flags` bit 1: a cancellation callback is still armed.
#[allow(dead_code)] // documents the layout; the value is inlined above / in `coroutine.ll`
pub(crate) const COMPLETION_FLAG_CANCEL_ARMED: u8 = 0b10;

/// `%TaskRecord` field indices (see `coroutine.ll`). The record is
/// `{ i8 state, i8 cancel, ptr frame, ptr waiter, ptr scheduler, ptr scope_next,
/// T result }`; the runtime only ever touches the header.
#[allow(dead_code)] // field 0; loaded directly through the record pointer
pub(crate) const TASK_RECORD_STATE: u32 = 0;
pub(crate) const TASK_RECORD_CANCEL: u32 = 1;
pub(crate) const TASK_RECORD_FRAME: u32 = 2;
#[allow(dead_code)] // written only by `coroutine.ll` (`__staple_task_await_register`)
pub(crate) const TASK_RECORD_WAITER: u32 = 3;
pub(crate) const TASK_RECORD_SCHEDULER: u32 = 4;
#[allow(dead_code)] // read/written only by `coroutine.ll` (scope teardown list)
pub(crate) const TASK_RECORD_SCOPE_NEXT: u32 = 5;
pub(crate) const TASK_RECORD_RESULT: u32 = 6;
/// `%TaskRecord.state`: 0 pending, 1 completed, 2 cancelled.
#[allow(dead_code)] // documents the runtime layout; `coroutine.ll` uses the literal
pub(crate) const TASK_STATE_CANCELLED: u64 = 2;

/// The storage of a lowered sum value: one `i32` tag plus a payload buffer
/// aligned for the widest alternative. Target layout determines the alignment.
#[derive(Clone)]
pub(crate) struct SumStorage<'context> {
    pub(crate) tag: inkwell::values::PointerValue<'context>,
    pub(crate) payload: inkwell::values::PointerValue<'context>,
    pub(crate) alignment: u32,
}

/// The descriptor's layout words. Store size and array stride are distinct:
/// a target ABI can pad an allocation beyond the bytes copied for a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DescriptorLayout {
    pub store_size: u64,
    pub stride: u64,
    pub align: u32,
}

impl DescriptorLayout {
    fn product(fields: &[Self]) -> Self {
        let align = fields.iter().map(|field| field.align).max().unwrap_or(1);
        let mut end = 0u64;
        for field in fields {
            end = end.div_ceil(field.align as u64) * field.align as u64;
            end += field.store_size;
        }
        let store_size = end.div_ceil(align as u64) * align as u64;
        Self {
            store_size,
            stride: store_size,
            align,
        }
    }

    /// Mirror `compile_sum_type`, including its initial i8 carrier and the
    /// tag's alignment. Even a sum of byte-aligned alternatives aligns to i32.
    fn sum(alternatives: &[Self], tag: Self) -> (Self, u64) {
        let mut carrier = Self {
            store_size: 1,
            stride: 1,
            align: 1,
        };
        let mut maximum_size = 0;
        for alternative in alternatives {
            maximum_size = maximum_size.max(alternative.store_size);
            if alternative.align > carrier.align {
                carrier = *alternative;
            }
        }
        let length = maximum_size.max(1).div_ceil(carrier.store_size.max(1));
        let offset = tag.store_size.div_ceil(carrier.align as u64) * carrier.align as u64;
        let align = tag.align.max(carrier.align);
        let store_size = (offset + length * carrier.stride).div_ceil(align as u64) * align as u64;
        (
            Self {
                store_size,
                stride: store_size,
                align,
            },
            offset,
        )
    }
}

/// Layout words carried by a descriptor in generated code.
#[derive(Clone, Copy)]
pub(crate) struct RuntimeDescriptorLayout<'context> {
    pub store_size: inkwell::values::IntValue<'context>,
    pub stride: inkwell::values::IntValue<'context>,
    pub align: inkwell::values::IntValue<'context>,
}

/// The layout facts `compile_type` needs, sourced from the lowered program
/// (`LoweredSemanticIds`, the type catalog, and `runtime_opaque_kind`) so the
/// the emitter and lowered emitters cannot disagree.
#[derive(Clone, Copy)]
pub(crate) struct LayoutContext<'program> {
    view: EmissionView<'program>,
}

impl<'program> LayoutContext<'program> {
    pub(crate) fn new(view: EmissionView<'program>) -> Self {
        Self { view }
    }

    /// The checked representation of the standard library `String` type, or
    /// `None` when the program has no standard library.
    pub(crate) fn string_representation(&self) -> Option<&'program CheckedType> {
        self.view.semantic_ids().string_representation.as_ref()
    }

    /// The catalog's concrete `Copy` decision for a fully substituted type.
    pub(crate) fn is_copy(&self, value_type: &CheckedType) -> bool {
        self.view.concrete_is_copy(value_type)
    }

    /// Whether the type is the standard library `IO` resource type, which is
    /// represented as an empty struct (the resource is pass-through).
    pub(crate) fn is_io(&self, value_type: &CheckedType) -> bool {
        opaque_is(value_type, self.view.semantic_ids().io_type)
    }

    /// Whether the type is the standard library `Reactive` scope handle,
    /// represented as a pointer to its runtime record.
    pub(crate) fn is_reactive(&self, value_type: &CheckedType) -> bool {
        opaque_is(value_type, self.view.semantic_ids().reactive_type)
    }

    /// Whether the type is one of the runtime handles represented as a pointer:
    /// coroutines (their GC frame), tasks (`%TaskRecord`), schedulers / task
    /// scopes (their runtime records), and `Wait` / `Resolver` /
    /// `CompletionToken` (the shared `%Completion` record).
    pub(crate) fn is_pointer_runtime(&self, value_type: &CheckedType) -> bool {
        if self.view.runtime_opaque_kind(value_type).is_some() {
            return true;
        }
        let ids = self.view.semantic_ids();
        opaque_is(value_type, ids.task_type) || opaque_is(value_type, ids.tasks_type)
    }
}

/// Whether the type is the opaque type `expected` names. A program without
/// that semantic ID (no standard library) has no such type, so an absent ID
/// never matches, whatever the type.
fn opaque_is(value_type: &CheckedType, expected: Option<TypeId>) -> bool {
    matches!(
        (value_type, expected),
        (CheckedType::Opaque { id, .. }, Some(expected)) if *id == expected
    )
}

impl<'program, 'context> Backend<'program, 'context> {
    pub(crate) fn compile_type(
        &self,
        value_type: &CheckedType,
    ) -> CodeGenerationResult<BasicTypeEnum<'context>> {
        match value_type {
            CheckedType::Forall(_) => Ok(self.closure_type().into()),
            CheckedType::Inferred => Err(Diagnostic::new(
                Span::Compiler,
                "cannot generate code for an inferred type before type checking",
            )),
            CheckedType::Error => Err(Diagnostic::new(
                Span::Compiler,
                "cannot generate code for an erroneous type",
            )),
            CheckedType::Never => Ok(self.context.struct_type(&[], false).into()),
            CheckedType::NumberLiteral(_) => {
                Ok(self.compile_integer_type(IntegerType::USize).into())
            }
            CheckedType::CChar => Ok(self.context.i8_type().into()),
            CheckedType::Parameter { name, .. } => Err(Diagnostic::new(
                Span::Compiler,
                format!("cannot generate code for unspecialized type parameter `{name}`"),
            )),
            CheckedType::TypeConstructor { name, .. } => Err(Diagnostic::new(
                Span::Compiler,
                format!("cannot generate code for partially applied type `{name}`"),
            )),
            CheckedType::Opaque { .. } if self.layout.is_io(value_type) => {
                Ok(self.context.struct_type(&[], false).into())
            }
            CheckedType::Opaque { .. } if self.layout.is_reactive(value_type) => {
                Ok(self.context.ptr_type(AddressSpace::default()).into())
            }
            CheckedType::Opaque { .. } if self.layout.is_pointer_runtime(value_type) => {
                // A coroutine value is a pointer to its GC-allocated frame; a
                // task handle points to its `%TaskRecord`; a `Wait` / `Resolver`
                // / `CompletionToken` all point to the shared `%Completion`
                // record; a scheduler / task scope point to their runtime
                // records.
                Ok(self.context.ptr_type(AddressSpace::default()).into())
            }
            CheckedType::Opaque { .. }
                if self
                    .layout
                    .view
                    .recursive_representation(value_type)
                    .is_some() =>
            {
                self.compile_type(
                    &self
                        .layout
                        .view
                        .recursive_representation(value_type)
                        .unwrap(),
                )
            }
            CheckedType::Opaque { name, .. } => Err(Diagnostic::new(
                Span::Compiler,
                format!("opaque type `{name}` has no by-value representation"),
            )),
            CheckedType::CString => Ok(self.context.ptr_type(AddressSpace::default()).into()),
            CheckedType::String | CheckedType::StringLiteralSet(_) => self
                .layout
                .string_representation()
                .ok_or_else(|| {
                    Diagnostic::new(
                        Span::Compiler,
                        "standard library String representation was not checked",
                    )
                })
                .and_then(|representation| self.compile_type(representation)),
            CheckedType::RawOpaquePtr
            | CheckedType::RawOpaqueMutPtr
            | CheckedType::RawPtr { .. }
            | CheckedType::RawMutPtr { .. } => {
                Ok(self.context.ptr_type(AddressSpace::default()).into())
            }
            CheckedType::Function(_) => Ok(self.closure_type().into()),
            CheckedType::Product(product) => self.compile_product_type(product).map(Into::into),
            CheckedType::Sum(sum) => self.compile_sum_type(sum).map(Into::into),
            CheckedType::I8 => Ok(self.compile_integer_type(IntegerType::I8).into()),
            CheckedType::I16 => Ok(self.compile_integer_type(IntegerType::I16).into()),
            CheckedType::I32 => Ok(self.compile_integer_type(IntegerType::I32).into()),
            CheckedType::I64 => Ok(self.compile_integer_type(IntegerType::I64).into()),
            CheckedType::U8 => Ok(self.compile_integer_type(IntegerType::U8).into()),
            CheckedType::U16 => Ok(self.compile_integer_type(IntegerType::U16).into()),
            CheckedType::U32 => Ok(self.compile_integer_type(IntegerType::U32).into()),
            CheckedType::U64 => Ok(self.compile_integer_type(IntegerType::U64).into()),
            CheckedType::ISize => Ok(self.compile_integer_type(IntegerType::ISize).into()),
            CheckedType::USize => Ok(self.compile_integer_type(IntegerType::USize).into()),
            CheckedType::F32 => Ok(self.compile_float_type(FloatType::F32).into()),
            CheckedType::F64 => Ok(self.compile_float_type(FloatType::F64).into()),
            CheckedType::Wrapper { representation, .. } => self.compile_type(representation),
        }
    }

    pub(crate) fn descriptor_layout(
        &self,
        value_type: &CheckedType,
    ) -> CodeGenerationResult<DescriptorLayout> {
        let layout = match value_type {
            CheckedType::Product(product) => {
                let fields = product
                    .iter_positions()
                    .map(|field| self.descriptor_layout(&field.value_type))
                    .collect::<CodeGenerationResult<Vec<_>>>()?;
                DescriptorLayout::product(&fields)
            }
            CheckedType::Sum(sum) => {
                let alternatives = sum
                    .alternatives
                    .iter()
                    .map(|alternative| self.descriptor_layout(alternative))
                    .collect::<CodeGenerationResult<Vec<_>>>()?;
                let tag = self.descriptor_layout(&CheckedType::I32)?;
                DescriptorLayout::sum(&alternatives, tag).0
            }
            CheckedType::Wrapper { representation, .. } => {
                self.descriptor_layout(representation)?
            }
            _ => {
                let ty = self.compile_type(value_type)?;
                DescriptorLayout {
                    store_size: self.target_data.get_store_size(&ty),
                    stride: self.target_data.get_abi_size(&ty),
                    align: self.target_data.get_abi_alignment(&ty),
                }
            }
        };
        Ok(layout)
    }

    /// The fixed descriptor prefix is shared by static constants and stack-built
    /// compound descriptors. Component pointers follow in semantic field order.
    pub(crate) fn type_descriptor_type(&self, count: u32) -> inkwell::types::StructType<'context> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        self.context.struct_type(
            &[
                self.size_type.into(),
                self.size_type.into(),
                self.size_type.into(),
                self.context.i32_type().into(),
                pointer.into(),
                self.context.i32_type().into(),
                pointer.array_type(count).into(),
            ],
            false,
        )
    }

    /// Read the layout prefix through the common descriptor header, independent
    /// of the number of trailing components.
    pub(crate) fn load_descriptor_layout(
        &self,
        descriptor: inkwell::values::PointerValue<'context>,
    ) -> CodeGenerationResult<RuntimeDescriptorLayout<'context>> {
        let mut words = Vec::new();
        for index in 0..3 {
            let pointer = self
                .builder
                .build_struct_gep(
                    self.type_descriptor_type(0),
                    descriptor,
                    index,
                    "descriptor.layout.word",
                )
                .map_err(super::compiler_diagnostic)?;
            words.push(
                self.builder
                    .build_load(self.size_type, pointer, "descriptor.layout")
                    .map_err(super::compiler_diagnostic)?
                    .into_int_value(),
            );
        }
        Ok(RuntimeDescriptorLayout {
            store_size: words[0],
            stride: words[1],
            align: words[2],
        })
    }

    #[allow(dead_code)]
    pub(crate) fn runtime_product_layout(
        &self,
        fields: &[RuntimeDescriptorLayout<'context>],
    ) -> CodeGenerationResult<RuntimeDescriptorLayout<'context>> {
        let mut size = self.size_type.const_zero();
        let mut align = self.size_type.const_int(1, false);
        for field in fields {
            size = self.runtime_align_up(size, field.align)?;
            size = self
                .builder
                .build_int_add(size, field.store_size, "product.size")
                .map_err(super::compiler_diagnostic)?;
            let larger = self
                .builder
                .build_int_compare(
                    inkwell::IntPredicate::UGT,
                    field.align,
                    align,
                    "product.wider",
                )
                .map_err(super::compiler_diagnostic)?;
            align = self
                .builder
                .build_select(larger, field.align, align, "product.align")
                .map_err(super::compiler_diagnostic)?
                .into_int_value();
        }
        size = self.runtime_align_up(size, align)?;
        Ok(RuntimeDescriptorLayout {
            store_size: size,
            stride: size,
            align,
        })
    }

    pub(crate) fn runtime_align_up(
        &self,
        value: inkwell::values::IntValue<'context>,
        alignment: inkwell::values::IntValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        let fail = super::compiler_diagnostic;
        let padding = self
            .builder
            .build_int_sub(
                alignment,
                self.size_type.const_int(1, false),
                "layout.padding",
            )
            .map_err(fail)?;
        let padded = self
            .builder
            .build_int_add(value, padding, "layout.padded")
            .map_err(fail)?;
        let units = self
            .builder
            .build_int_unsigned_div(padded, alignment, "layout.units")
            .map_err(fail)?;
        self.builder
            .build_int_mul(units, alignment, "layout.aligned")
            .map_err(fail)
    }

    /// Preserve the concrete sum's first-widest carrier selection. Its store
    /// size controls the element count, while its stride controls allocation.
    #[allow(dead_code)]
    pub(crate) fn runtime_sum_layout(
        &self,
        alternatives: &[RuntimeDescriptorLayout<'context>],
    ) -> CodeGenerationResult<(
        RuntimeDescriptorLayout<'context>,
        inkwell::values::IntValue<'context>,
    )> {
        use inkwell::IntPredicate;
        let fail = super::compiler_diagnostic;
        let one = self.size_type.const_int(1, false);
        let maximum = |left, right| -> CodeGenerationResult<_> {
            let larger = self
                .builder
                .build_int_compare(IntPredicate::UGT, left, right, "layout.larger")
                .map_err(fail)?;
            Ok(self
                .builder
                .build_select(larger, left, right, "layout.maximum")
                .map_err(fail)?
                .into_int_value())
        };
        let round_up = |size, alignment| -> CodeGenerationResult<_> {
            let padding = self
                .builder
                .build_int_sub(alignment, one, "layout.padding")
                .map_err(fail)?;
            let padded = self
                .builder
                .build_int_add(size, padding, "layout.padded")
                .map_err(fail)?;
            let count = self
                .builder
                .build_int_unsigned_div(padded, alignment, "layout.units")
                .map_err(fail)?;
            Ok(self
                .builder
                .build_int_mul(count, alignment, "layout.rounded")
                .map_err(fail)?)
        };
        let mut carrier = RuntimeDescriptorLayout {
            store_size: one,
            stride: one,
            align: one,
        };
        let mut size = one;
        for alternative in alternatives {
            size = maximum(size, alternative.store_size)?;
            let wider = self
                .builder
                .build_int_compare(
                    IntPredicate::UGT,
                    alternative.align,
                    carrier.align,
                    "sum.wider",
                )
                .map_err(fail)?;
            let select = |new, old| -> CodeGenerationResult<_> {
                Ok(self
                    .builder
                    .build_select(wider, new, old, "sum.carrier")
                    .map_err(fail)?
                    .into_int_value())
            };
            carrier = RuntimeDescriptorLayout {
                store_size: select(alternative.store_size, carrier.store_size)?,
                stride: select(alternative.stride, carrier.stride)?,
                align: select(alternative.align, carrier.align)?,
            };
        }
        let carrier_size = maximum(carrier.store_size, one)?;
        let rounded = round_up(size, carrier_size)?;
        let count = self
            .builder
            .build_int_unsigned_div(rounded, carrier_size, "sum.count")
            .map_err(fail)?;
        let payload = self
            .builder
            .build_int_mul(count, carrier.stride, "sum.payload.size")
            .map_err(fail)?;
        let tag = self.descriptor_layout(&CheckedType::I32)?;
        let offset = round_up(
            self.size_type.const_int(tag.store_size, false),
            carrier.align,
        )?;
        let align = maximum(
            carrier.align,
            self.size_type.const_int(tag.align as u64, false),
        )?;
        let size = self
            .builder
            .build_int_add(offset, payload, "sum.unpadded.size")
            .map_err(fail)?;
        let size = round_up(size, align)?;
        Ok((
            RuntimeDescriptorLayout {
                store_size: size,
                stride: size,
                align,
            },
            offset,
        ))
    }

    /// The runtime words of a statically known layout, for pointer-like types
    /// whose representation does not depend on their type arguments.
    pub(crate) fn static_runtime_layout(
        &self,
        layout: DescriptorLayout,
    ) -> RuntimeDescriptorLayout<'context> {
        RuntimeDescriptorLayout {
            store_size: self.size_type.const_int(layout.store_size, false),
            stride: self.size_type.const_int(layout.stride, false),
            align: self.size_type.const_int(layout.align as u64, false),
        }
    }

    /// Materialize a compound descriptor in a fixed-size entry allocation.
    /// The caller supplies its planned drop entry point; a null entry means
    /// the value requires no cleanup. These descriptors belong to this call
    /// frame and must be copied to persistent storage before a GC escape.
    #[allow(dead_code)]
    pub(crate) fn build_runtime_descriptor(
        &self,
        layout: RuntimeDescriptorLayout<'context>,
        components: &[inkwell::values::PointerValue<'context>],
        drop: inkwell::values::PointerValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let fail = super::compiler_diagnostic;
        let count = u32::try_from(components.len())
            .map_err(|_| Diagnostic::new(Span::Compiler, "too many type descriptor components"))?;
        let ty = self.type_descriptor_type(count);
        let slot = self
            .entry_builder()
            .build_alloca(ty, "type.descriptor")
            .map_err(fail)?;
        let no_drop = self
            .builder
            .build_is_null(drop, "descriptor.no.drop")
            .map_err(fail)?;
        let flags = self
            .builder
            .build_int_z_extend(no_drop, self.context.i32_type(), "descriptor.flags")
            .map_err(fail)?;
        let words: [inkwell::values::BasicValueEnum<'context>; 6] = [
            layout.store_size.into(),
            layout.stride.into(),
            layout.align.into(),
            flags.into(),
            drop.into(),
            self.context
                .i32_type()
                .const_int(count as u64, false)
                .into(),
        ];
        for (index, word) in words.into_iter().enumerate() {
            let field = self
                .builder
                .build_struct_gep(ty, slot, index as u32, "descriptor.field")
                .map_err(fail)?;
            self.builder.build_store(field, word).map_err(fail)?;
        }
        let array = self
            .builder
            .build_struct_gep(ty, slot, 6, "descriptor.components")
            .map_err(fail)?;
        let array_type = self
            .context
            .ptr_type(AddressSpace::default())
            .array_type(count);
        for (index, component) in components.iter().enumerate() {
            let field = unsafe {
                self.builder.build_gep(
                    array_type,
                    array,
                    &[
                        self.context.i32_type().const_zero(),
                        self.context.i32_type().const_int(index as u64, false),
                    ],
                    "descriptor.component",
                )
            }
            .map_err(fail)?;
            self.builder.build_store(field, *component).map_err(fail)?;
        }
        Ok(slot)
    }

    /// Components start at the same offset for every descriptor shape.
    #[allow(dead_code)]
    pub(crate) fn load_descriptor_component(
        &self,
        descriptor: inkwell::values::PointerValue<'context>,
        index: u32,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let fail = super::compiler_diagnostic;
        let pointer = self.context.ptr_type(AddressSpace::default());
        let array = self
            .builder
            .build_struct_gep(
                self.type_descriptor_type(0),
                descriptor,
                6,
                "descriptor.components",
            )
            .map_err(fail)?;
        let field = unsafe {
            self.builder.build_gep(
                pointer,
                array,
                &[self.size_type.const_int(index as u64, false)],
                "descriptor.component",
            )
        }
        .map_err(fail)?;
        Ok(self
            .builder
            .build_load(pointer, field, "descriptor.component.value")
            .map_err(fail)?
            .into_pointer_value())
    }

    pub(crate) fn compile_integer_type(
        &self,
        integer: IntegerType,
    ) -> inkwell::types::IntType<'context> {
        match integer {
            IntegerType::I8 | IntegerType::U8 => self.context.i8_type(),
            IntegerType::I16 | IntegerType::U16 => self.context.i16_type(),
            IntegerType::I32 | IntegerType::U32 => self.context.i32_type(),
            IntegerType::I64 | IntegerType::U64 => self.context.i64_type(),
            IntegerType::ISize | IntegerType::USize => self.size_type,
        }
    }

    pub(crate) fn compile_float_type(
        &self,
        float: FloatType,
    ) -> inkwell::types::FloatType<'context> {
        match float {
            FloatType::F32 => self.context.f32_type(),
            FloatType::F64 => self.context.f64_type(),
        }
    }

    pub(crate) fn closure_type(&self) -> inkwell::types::StructType<'context> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        self.context
            .struct_type(&[pointer.into(), pointer.into()], false)
    }

    pub(crate) fn compile_product_type(
        &self,
        product: &CheckedProductType,
    ) -> CodeGenerationResult<inkwell::types::StructType<'context>> {
        // A homogeneous product is one `[n x T]` field so a million-element
        // array stays O(1). Mixed products keep their historical flat layout
        // (one field per position) to avoid changing unrelated ABIs.
        if let [run] = product.runs.as_slice()
            && let Some(count) = run.positions()
            && count > 1
        {
            let element_type = self.compile_type(&run.element.value_type)?;
            return Ok(self
                .context
                .struct_type(&[element_type.array_type(count as u32).into()], false));
        }
        let field_types = product
            .runs
            .iter()
            .flat_map(|run| {
                let count = run.positions().unwrap_or(1);
                std::iter::repeat_n(&run.element.value_type, count)
            })
            .map(|value_type| self.compile_type(value_type).map(Into::into))
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        Ok(self.context.struct_type(&field_types, false))
    }

    pub(crate) fn compile_sum_type(
        &self,
        sum: &crate::CheckedSumType,
    ) -> CodeGenerationResult<inkwell::types::StructType<'context>> {
        let mut maximum_size = 0;
        let mut carrier = self.context.i8_type().into();
        let mut maximum_alignment = 1;
        for alternative in &sum.alternatives {
            let alternative_type = self.compile_type(alternative)?;
            let size = self.target_data.get_store_size(&alternative_type);
            let alignment = self.target_data.get_abi_alignment(&alternative_type);
            maximum_size = maximum_size.max(size);
            if alignment > maximum_alignment {
                maximum_alignment = alignment;
                carrier = alternative_type;
            }
        }
        let carrier_size = self.target_data.get_store_size(&carrier).max(1);
        let length = maximum_size.max(1).div_ceil(carrier_size) as u32;
        // An aggregate carrier can contain padding. LLVM does not preserve
        // those bytes when loading/returning an SSA aggregate, but another
        // alternative may put real data there. Keep the existing carrier's
        // size/alignment rule while making every payload byte an SSA field.
        let payload: BasicTypeEnum<'context> = if carrier.is_struct_type() {
            let bytes = u32::try_from(u64::from(length) * self.target_data.get_abi_size(&carrier))
                .map_err(|_| {
                    Diagnostic::new(
                        Span::Compiler,
                        "sum payload exceeds LLVM's array length limit",
                    )
                })?;
            self.context
                .struct_type(
                    &[
                        carrier.array_type(0).into(),
                        self.context.i8_type().array_type(bytes).into(),
                    ],
                    false,
                )
                .into()
        } else {
            carrier.array_type(length).into()
        };
        Ok(self
            .context
            .struct_type(&[self.context.i32_type().into(), payload.into()], false))
    }

    pub(crate) fn buffer_header_type(
        &self,
        element: BasicTypeEnum<'context>,
    ) -> inkwell::types::StructType<'context> {
        self.context.struct_type(
            &[self.size_type.into(), self.size_type.into(), element],
            false,
        )
    }

    /// The fixed frame-header prefix every coroutine frame starts with; used to
    /// Runtime resume status: a kind byte and pending pointer.
    pub(crate) fn coroutine_status_type(&self) -> inkwell::types::StructType<'context> {
        self.context.struct_type(
            &[
                self.context.i8_type().into(),
                self.context.ptr_type(AddressSpace::default()).into(),
            ],
            false,
        )
    }

    /// GEP a header field through a `ptr` whose full frame type is not known at
    /// the site (`await`, `block_on`, drop). Matches `coroutine.ll`'s
    /// `%CoroHeader` plus the trailing `resources` pointer.
    pub(crate) fn coroutine_header_type(&self) -> inkwell::types::StructType<'context> {
        let ptr: BasicTypeEnum<'context> = self.context.ptr_type(AddressSpace::default()).into();
        self.context.struct_type(
            &[
                self.context.i8_type().into(),
                ptr,
                ptr,
                ptr,
                ptr,
                ptr,
                ptr,
                ptr,
                ptr,
                ptr,
            ],
            false,
        )
    }

    /// The full `%TaskRecord` layout for a `spawn`ed task whose result type
    /// lowers to `result_llvm`. Field indices are the `TASK_RECORD_*` constants.
    pub(crate) fn task_record_type(
        &self,
        result_llvm: BasicTypeEnum<'context>,
    ) -> inkwell::types::StructType<'context> {
        let i8_type = self.context.i8_type();
        let ptr = self.context.ptr_type(AddressSpace::default());
        self.context.struct_type(
            &[
                i8_type.into(),
                i8_type.into(),
                ptr.into(),
                ptr.into(),
                ptr.into(),
                ptr.into(),
                result_llvm,
            ],
            false,
        )
    }

    /// The `%TaskRecord` header (everything the runtime touches), for GEPs
    /// through a `ptr` whose result type is not known at the site.
    pub(crate) fn task_record_header_type(&self) -> inkwell::types::StructType<'context> {
        let i8_type = self.context.i8_type();
        let ptr = self.context.ptr_type(AddressSpace::default());
        self.context.struct_type(
            &[
                i8_type.into(),
                i8_type.into(),
                ptr.into(),
                ptr.into(),
                ptr.into(),
                ptr.into(),
            ],
            false,
        )
    }

    /// The full `%Completion` layout for a completion whose value type lowers to
    /// `value_llvm`. Field indices are the `COMPLETION_*` constants.
    pub(crate) fn completion_record_type(
        &self,
        value_llvm: BasicTypeEnum<'context>,
    ) -> inkwell::types::StructType<'context> {
        let i8_type = self.context.i8_type();
        let ptr = self.context.ptr_type(AddressSpace::default());
        self.context.struct_type(
            &[
                i8_type.into(),        // state
                i8_type.into(),        // flags
                self.size_type.into(), // generation
                ptr.into(),            // scheduler
                ptr.into(),            // waiter
                ptr.into(),            // cancel_env
                ptr.into(),            // cancel_fn
                value_llvm,            // value
            ],
            false,
        )
    }

    /// `%UntilFrame` — a `%CoroHeader` (10 fields) plus, at indices 10..=16:
    /// completion, reaction, payload, predicate code, predicate env, reactive
    /// scope, runner fn. Matches `coroutine.ll`'s `%UntilFrame`.
    pub(crate) fn until_frame_type(&self) -> inkwell::types::StructType<'context> {
        let ptr: BasicTypeEnum<'context> = self.context.ptr_type(AddressSpace::default()).into();
        let mut fields = vec![self.context.i8_type().into()];
        fields.extend(std::iter::repeat(ptr).take(16));
        self.context.struct_type(&fields, false)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        CheckedType, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker, TypeId,
    };

    use super::{LayoutContext, opaque_is};

    fn standard_library_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent")
            .join("stdlib")
    }

    /// Every concrete type an instance body's signature and bindings name:
    /// result, parameters, resources, captures, and parameters.
    /// An absent semantic ID never matches: without it, `None == None` would
    /// classify every non-opaque type as `IO`/`Reactive`/a task handle.
    #[test]
    fn an_absent_semantic_id_matches_no_type() {
        let opaque = CheckedType::Opaque {
            sized: false,
            id: TypeId(7),
            name: "Handle".to_owned(),
            arguments: Vec::new(),
        };
        assert!(!opaque_is(&CheckedType::I32, None));
        assert!(!opaque_is(&opaque, None));
        assert!(!opaque_is(&CheckedType::I32, Some(TypeId(7))));
        assert!(!opaque_is(&opaque, Some(TypeId(8))));
        assert!(opaque_is(&opaque, Some(TypeId(7))));
    }

    fn signature_types(module: &LoweredModule) -> Vec<CheckedType> {
        let view = module.program();
        let mut types = Vec::new();
        for (id, _) in view.instances().collect::<Vec<_>>() {
            if let Some(signature) = view.instance_signature(id) {
                types.push(signature.result.as_ref().clone());
                match signature.parameter.as_ref() {
                    CheckedType::Product(product) => {
                        types.extend(product.iter_positions().map(|e| e.value_type.clone()))
                    }
                    other => types.push(other.clone()),
                }
                types.extend(
                    signature
                        .effects
                        .resources
                        .iter()
                        .map(|resource| resource.value_type.clone()),
                );
            }
            for capture in view.instance_captures(id).unwrap_or(&[]) {
                types.push(capture.value_type.clone());
            }
            for parameter in view.instance_parameters(id).unwrap_or(&[]) {
                types.push(parameter.value_type.clone());
            }
        }
        types
    }

    #[test]
    fn descriptor_sum_layout_keeps_the_first_maximally_aligned_carrier() {
        use super::DescriptorLayout as L;
        let tag = L {
            store_size: 4,
            stride: 4,
            align: 4,
        };
        let byte = L {
            store_size: 1,
            stride: 1,
            align: 1,
        };
        assert_eq!(
            L::sum(&[byte], tag),
            (
                L {
                    store_size: 8,
                    stride: 8,
                    align: 4
                },
                4
            )
        );
        let eight = L {
            store_size: 8,
            stride: 8,
            align: 4,
        };
        let twelve = L {
            store_size: 12,
            stride: 12,
            align: 4,
        };
        assert_eq!(L::sum(&[eight, twelve], tag).0.store_size, 20);
        assert_eq!(L::sum(&[twelve, eight], tag).0.store_size, 16);
        let wide = L {
            store_size: 8,
            stride: 8,
            align: 8,
        };
        let packed = L {
            store_size: 17,
            stride: 17,
            align: 1,
        };
        assert_eq!(
            L::sum(&[packed, wide], tag),
            (
                L {
                    store_size: 32,
                    stride: 32,
                    align: 8
                },
                8
            )
        );
    }

    #[test]
    fn dynamic_copy_and_drop_use_descriptor_entries() {
        use super::Backend;
        use crate::codegen::create_target_machine;
        use inkwell::{AddressSpace, OptimizationLevel};
        let root = standard_library_root();
        let loaded = ProgramLoader::new()
            .with_standard_library_root(root.clone())
            .load_source("let value = 1", &root)
            .unwrap();
        let typed = TypeChecker::new()
            .check(NameResolver::new().resolve_program(loaded).unwrap())
            .unwrap();
        let module = Lowerer::new().lower(&typed).unwrap();
        let context = inkwell::context::Context::create();
        let target = create_target_machine(None).unwrap();
        let backend = Backend::new(&context, &target, LayoutContext::new(module.program()));
        let pointer = context.ptr_type(AddressSpace::default());
        let drop = backend.llvm_module.add_function(
            "drop",
            context
                .void_type()
                .fn_type(&[pointer.into(), pointer.into()], false),
            None,
        );
        backend
            .builder
            .position_at_end(context.append_basic_block(drop, "entry"));
        let value = drop.get_nth_param(1).unwrap().into_pointer_value();
        let byte = backend
            .builder
            .build_load(context.i8_type(), value, "byte")
            .unwrap()
            .into_int_value();
        let increment = backend
            .builder
            .build_int_add(byte, context.i8_type().const_int(1, false), "increment")
            .unwrap();
        backend.builder.build_store(value, increment).unwrap();
        backend.builder.build_return(None).unwrap();
        let function = backend.llvm_module.add_function(
            "copy_drop",
            context
                .void_type()
                .fn_type(&[pointer.into(), pointer.into(), pointer.into()], false),
            None,
        );
        backend
            .builder
            .position_at_end(context.append_basic_block(function, "entry"));
        let descriptor = function.get_nth_param(0).unwrap().into_pointer_value();
        let source = function.get_nth_param(1).unwrap().into_pointer_value();
        let destination = function.get_nth_param(2).unwrap().into_pointer_value();
        backend
            .copy_dynamic_value(descriptor, source, destination)
            .unwrap();
        backend.drop_dynamic_value(descriptor, destination).unwrap();
        backend.builder.build_return(None).unwrap();
        backend.llvm_module.verify().unwrap();
        let engine = backend
            .llvm_module
            .create_jit_execution_engine(OptimizationLevel::None)
            .unwrap();
        #[repr(C)]
        struct Descriptor {
            store_size: usize,
            stride: usize,
            align: usize,
            flags: u32,
            drop: usize,
            count: u32,
        }
        type CopyDrop = unsafe extern "C" fn(*const Descriptor, *const u8, *mut u8);
        let call = unsafe { engine.get_function::<CopyDrop>("copy_drop") }.unwrap();
        let drop = engine.get_function_address("drop").unwrap();
        for drop in [0, drop] {
            // Store size differs from stride: bytes after the stored value must
            // remain untouched, and the source may be unaligned.
            let descriptor = Descriptor {
                store_size: 3,
                stride: 8,
                align: 8,
                flags: u32::from(drop == 0),
                drop,
                count: 0,
            };
            let source = [99, 10, 20, 30, 99];
            let mut destination = [77; 10];
            unsafe {
                call.call(
                    &descriptor,
                    source.as_ptr().add(1),
                    destination.as_mut_ptr().add(1),
                );
            }
            let mut expected = [77; 10];
            expected[1..4].copy_from_slice(&[10 + u8::from(drop != 0), 20, 30]);
            assert_eq!(destination, expected);
            assert_eq!(source, [99, 10, 20, 30, 99]);
        }
    }

    #[test]
    fn runtime_layout_calculations_match_concrete_layouts() {
        use super::{Backend, DescriptorLayout};
        use crate::codegen::create_target_machine;
        use inkwell::OptimizationLevel;
        let root = standard_library_root();
        let loaded = ProgramLoader::new()
            .with_standard_library_root(root.clone())
            .load_source(
                "def choose: (I32, I32) -> I32 = (a, b) => a\nlet value = choose (1, 2)",
                &root,
            )
            .unwrap();
        let typed = TypeChecker::new()
            .check(NameResolver::new().resolve_program(loaded).unwrap())
            .unwrap();
        let module = Lowerer::new().lower(&typed).unwrap();
        let context = inkwell::context::Context::create();
        let target = create_target_machine(None).unwrap();
        let backend = Backend::new(&context, &target, LayoutContext::new(module.program()));
        let mut signature = module.program().instances().find_map(|(id, _)| {
            let signature = module.program().instance_signature(id)?;
            matches!(signature.parameter.as_ref(), CheckedType::Product(product) if product.position_count() == 2)
                .then(|| signature.clone())
        }).expect("two-argument instance");
        assert_eq!(
            backend
                .compile_dynamic_function_type(&signature, 2)
                .unwrap()
                .count_param_types(),
            6 + signature.effects.resources.len() as u32
        );
        signature.mutations = vec![crate::CheckedMutation::Whole];
        assert_eq!(
            backend
                .compile_dynamic_function_type(&signature, 2)
                .unwrap()
                .count_param_types(),
            5 + signature.effects.resources.len() as u32
        );
        signature.parameter = Box::new(CheckedType::I32);
        signature.effects.resources.push(crate::CheckedResource {
            value_type: CheckedType::I32,
            mutable: false,
        });
        assert!(
            backend
                .compile_dynamic_function_type(&signature, 2)
                .unwrap()
                .get_return_type()
                .is_none()
        );
        // Read actual descriptor pointers at execution time, rather than relying
        // on LLVM constant folding to test these calculations.
        let pointer = context.ptr_type(inkwell::AddressSpace::default());
        let function = backend.llvm_module.add_function(
            "layout",
            context
                .void_type()
                .fn_type(&[pointer.into(), pointer.into(), pointer.into()], false),
            None,
        );
        backend
            .builder
            .position_at_end(context.append_basic_block(function, "entry"));
        let left = backend
            .load_descriptor_layout(function.get_nth_param(0).unwrap().into_pointer_value())
            .unwrap();
        let right = backend
            .load_descriptor_layout(function.get_nth_param(1).unwrap().into_pointer_value())
            .unwrap();
        let product = backend.runtime_product_layout(&[left, right]).unwrap();
        let (sum, offset) = backend.runtime_sum_layout(&[left, right]).unwrap();
        let inputs = [
            function.get_nth_param(0).unwrap().into_pointer_value(),
            function.get_nth_param(1).unwrap().into_pointer_value(),
        ];
        let descriptor = backend
            .build_runtime_descriptor(product, &inputs, pointer.const_null())
            .unwrap();
        let built = backend.load_descriptor_layout(descriptor).unwrap();
        let mut header = Vec::new();
        for index in [3, 5] {
            let slot = backend
                .builder
                .build_struct_gep(
                    backend.type_descriptor_type(0),
                    descriptor,
                    index,
                    "header.slot",
                )
                .unwrap();
            let value = backend
                .builder
                .build_load(context.i32_type(), slot, "header.value")
                .unwrap()
                .into_int_value();
            header.push(
                backend
                    .builder
                    .build_int_z_extend(value, backend.size_type, "header.word")
                    .unwrap(),
            );
        }
        let mut component_matches = Vec::new();
        for (index, input) in inputs.into_iter().enumerate() {
            let component = backend
                .load_descriptor_component(descriptor, index as u32)
                .unwrap();
            let actual = backend
                .builder
                .build_ptr_to_int(component, backend.size_type, "component.address")
                .unwrap();
            let expected = backend
                .builder
                .build_ptr_to_int(input, backend.size_type, "input.address")
                .unwrap();
            let equal = backend
                .builder
                .build_int_compare(
                    inkwell::IntPredicate::EQ,
                    actual,
                    expected,
                    "component.matches",
                )
                .unwrap();
            component_matches.push(
                backend
                    .builder
                    .build_int_z_extend(equal, backend.size_type, "component.match.word")
                    .unwrap(),
            );
        }
        let output = function.get_nth_param(2).unwrap().into_pointer_value();
        for (index, value) in [
            product.store_size,
            product.stride,
            product.align,
            sum.store_size,
            sum.stride,
            sum.align,
            offset,
            built.store_size,
            built.stride,
            built.align,
            component_matches[0],
            component_matches[1],
            header[0],
            header[1],
        ]
        .into_iter()
        .enumerate()
        {
            let slot = unsafe {
                backend.builder.build_gep(
                    backend.size_type,
                    output,
                    &[backend.size_type.const_int(index as u64, false)],
                    "output",
                )
            }
            .unwrap();
            backend.builder.build_store(slot, value).unwrap();
        }
        backend.builder.build_return(None).unwrap();
        backend.llvm_module.verify().unwrap();
        let engine = backend
            .llvm_module
            .create_jit_execution_engine(OptimizationLevel::None)
            .unwrap();
        type Calculate = unsafe extern "C" fn(*const usize, *const usize, *mut usize);
        let calculate = unsafe { engine.get_function::<Calculate>("layout") }.unwrap();
        let layouts = [
            DescriptorLayout {
                store_size: 0,
                stride: 0,
                align: 1,
            },
            DescriptorLayout {
                store_size: 1,
                stride: 1,
                align: 1,
            },
            DescriptorLayout {
                store_size: 3,
                stride: 3,
                align: 1,
            },
            DescriptorLayout {
                store_size: 4,
                stride: 4,
                align: 4,
            },
            DescriptorLayout {
                store_size: 8,
                stride: 8,
                align: 8,
            },
            DescriptorLayout {
                store_size: 9,
                stride: 16,
                align: 8,
            },
            DescriptorLayout {
                store_size: 24,
                stride: 24,
                align: 8,
            },
        ];
        let tag = backend.descriptor_layout(&CheckedType::I32).unwrap();
        for left in layouts {
            for right in layouts {
                let words = |layout: DescriptorLayout| {
                    [
                        layout.store_size as usize,
                        layout.stride as usize,
                        layout.align as usize,
                    ]
                };
                let mut output = [0; 14];
                unsafe {
                    calculate.call(
                        words(left).as_ptr(),
                        words(right).as_ptr(),
                        output.as_mut_ptr(),
                    );
                }
                let product = DescriptorLayout::product(&[left, right]);
                let (sum, offset) = DescriptorLayout::sum(&[left, right], tag);
                assert_eq!(
                    output,
                    [
                        product.store_size as usize,
                        product.stride as usize,
                        product.align as usize,
                        sum.store_size as usize,
                        sum.stride as usize,
                        sum.align as usize,
                        offset as usize,
                        product.store_size as usize,
                        product.stride as usize,
                        product.align as usize,
                        1,
                        1,
                        1,
                        2
                    ],
                    "{left:?}, {right:?}"
                );
            }
        }
    }

    #[test]
    fn descriptor_layouts_match_target_data_for_the_corpus() {
        use super::{Backend, DescriptorLayout};
        use crate::codegen::{CorpusSource, codegen_corpus, create_target_machine};
        fn verify(backend: &Backend<'_, '_>, value_type: &CheckedType) {
            if matches!(
                value_type,
                CheckedType::Forall(_) | CheckedType::TypeConstructor { .. }
            ) || crate::contains_type_parameter(value_type)
            {
                return;
            }
            let ty = backend.compile_type(value_type).unwrap();
            let layout = backend.descriptor_layout(value_type).unwrap();
            assert_eq!(
                layout.store_size,
                backend.target_data.get_store_size(&ty),
                "store size: {value_type}"
            );
            assert_eq!(
                layout.stride,
                backend.target_data.get_abi_size(&ty),
                "stride: {value_type}"
            );
            assert_eq!(
                layout.align,
                backend.target_data.get_abi_alignment(&ty),
                "alignment: {value_type}"
            );
            match value_type {
                CheckedType::Product(product) => {
                    let mut offset = 0u64;
                    let llvm = ty.into_struct_type();
                    for (index, field) in product.iter_positions().enumerate() {
                        let field_layout = backend.descriptor_layout(&field.value_type).unwrap();
                        offset =
                            offset.div_ceil(field_layout.align as u64) * field_layout.align as u64;
                        let element_type = backend.compile_type(&field.value_type).unwrap();
                        let (llvm_field, array_index) = backend
                            .product_field_position(llvm, index)
                            .expect("product position");
                        let field_offset = backend
                            .target_data
                            .offset_of_element(&llvm, llvm_field)
                            .expect("product field");
                        let array_offset = array_index.unwrap_or(0) as u64
                            * backend.target_data.get_abi_size(&element_type);
                        assert_eq!(field_offset + array_offset, offset);
                        offset += field_layout.store_size;
                        verify(backend, &field.value_type);
                    }
                }
                CheckedType::Sum(sum) => {
                    let alternatives = sum
                        .alternatives
                        .iter()
                        .map(|alternative| backend.descriptor_layout(alternative).unwrap())
                        .collect::<Vec<_>>();
                    let tag = backend.descriptor_layout(&CheckedType::I32).unwrap();
                    let (_, payload_offset) = DescriptorLayout::sum(&alternatives, tag);
                    assert_eq!(
                        backend
                            .target_data
                            .offset_of_element(&ty.into_struct_type(), 1),
                        Some(payload_offset),
                        "payload: {value_type}"
                    );
                    for alternative in &sum.alternatives {
                        verify(backend, alternative);
                    }
                }
                CheckedType::Wrapper { representation, .. } => verify(backend, representation),
                _ => {}
            }
        }
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let context = inkwell::context::Context::create();
        let target = create_target_machine(None).unwrap();
        let mut checked = 0;
        let measurement_path = std::env::var("STAPLE_LAYOUT_MEASUREMENTS").ok();
        let mut measurements = std::collections::BTreeMap::new();
        fn contains_product(ty: &CheckedType) -> bool {
            match ty {
                CheckedType::Product(_) => true,
                CheckedType::Sum(sum) => sum.alternatives.iter().any(contains_product),
                CheckedType::Wrapper { representation, .. } => contains_product(representation),
                _ => false,
            }
        }
        for fixture in codegen_corpus() {
            let (source, root) = match fixture.source {
                CorpusSource::Inline(source) => (source.to_owned(), workspace.to_path_buf()),
                CorpusSource::File(path) => {
                    let full = workspace.join(path);
                    (
                        std::fs::read_to_string(&full).unwrap(),
                        full.parent().unwrap().to_path_buf(),
                    )
                }
            };
            let loaded = ProgramLoader::new()
                .with_standard_library_root(workspace.join("stdlib"))
                .load_source(&source, &root)
                .unwrap();
            let typed = TypeChecker::new()
                .check(NameResolver::new().resolve_program(loaded).unwrap())
                .unwrap();
            let module = Lowerer::new().lower(&typed).unwrap();
            let backend = Backend::new(&context, &target, LayoutContext::new(module.program()));
            let mut types = signature_types(&module);
            types.extend(module.program().initializer_layout_value_types().cloned());
            for (_, instance) in module.program().instances() {
                if let Some(body) = &instance.body {
                    types.extend(body.layout_value_types().cloned());
                }
            }
            for value_type in types {
                verify(&backend, &value_type);
                if measurement_path.is_some()
                    && contains_product(&value_type)
                    && !crate::contains_type_parameter(&value_type)
                {
                    if let Ok(ty) = backend.compile_type(&value_type) {
                        measurements.insert(
                            format!("{}:{value_type:?}", fixture.name),
                            backend.target_data.get_store_size(&ty),
                        );
                    }
                }
                checked += 1;
            }
        }
        assert!(checked > 1000, "the test must sweep the full corpus");
        if let Some(path) = measurement_path {
            let mut output = String::new();
            for (ty, size) in &measurements {
                output.push_str(&format!("{size}\t{ty}\n"));
            }
            std::fs::write(path, output).unwrap();
            eprintln!(
                "product-containing corpus types: {}, total store bytes: {}",
                measurements.len(),
                measurements.values().sum::<u64>()
            );
        }
    }

    /// The emission `LayoutContext` must select exactly the representations
    /// and pass modes the checker-based predicates selected: `Copy`, `IO`,
    /// `Reactive`, and the pointer-represented runtime handles. Divergence
    /// would silently change IR, so a fixture sweep locks the two sources
    /// together.
    #[test]
    fn layout_context_agrees_with_checker_predicates() {
        for source in [
            concat!(
                "def identity: <T where Copy T> T -> T = <T where Copy T> value => value\n",
                "def apply: (I32 -> I32) -> I32 -> I32 = f => value => f value\n",
                "let direct: I32 = identity 1\n",
                "let indirect: I32 = apply identity 1\n",
            ),
            concat!(
                "type Point wrap (I32, I32)\n",
                "def render: <T where Display T> move T -> String = <T where Display T> move value => \"value=$value\"\n",
                "let text: String = render 1\n",
                "let make: () -> ((I32, I32) -> Point) = () => Point\n",
                "let point = make () (1, 2)\n",
                "let debug = \"${(1, 2):?}\"\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "use std.coroutine.*\n",
                "def task: () -> Coroutine{}<I32> = () => coro { 1 }\n",
                "let created = task ()\n",
                "let signal count = 0\n",
                "let doubled = count + count\n",
                "reaction { () }\n",
            ),
            concat!(
                "use std.coroutine.*\n",
                "def observe: move Wait<I32> -> Coroutine{}<()> = move w => coro {\n",
                "    let outcome = await w\n",
                "    match outcome { Completed v => (), Cancelled() => () }\n",
                "}\n",
                "def make: Scheduler -> (wait: Wait<I32>, resolver: Resolver<I32>) = s => completion s\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "    let (w, r) = make sched\n",
                "    let _ = spawn (observe w)\n",
                "    Resolver.complete r 1\n",
                "    let _ = pump (sched, 4)\n",
                "}\n",
            ),
            concat!(
                "use std.coroutine.*\n",
                "def obs: move Wait<()> -> Coroutine{}<()> = move w => coro {\n",
                "    let outcome = await w\n",
                "    match outcome { Completed uu => (), Cancelled() => () }\n",
                "}\n",
                "def mk: Scheduler -> (wait: Wait<()>, token: CompletionToken) = s => completion_token s\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "    let (w, t) = mk sched\n",
                "    let _ = spawn (obs w)\n",
                "    let _ = pump (sched, 4)\n",
                "    CompletionToken.resolve t\n",
                "}\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "type Box<T> wrap (T)\n",
                "impl<T where Copy T> Drop Box<T> { cleanup = Box value => () }\n",
                "def take_i32: Box<I32> -> I32 = value => 1\n",
                "def take_cstring: Box<CString> -> I32 = value => 1\n",
                "def take_nested: Box<Box<I32>> -> I32 = value => 1\n",
                "def take_template: <T where Copy T> Box<T> -> I32 = <T where Copy T> value => 1\n",
                "let first = take_i32 (Box 1)\n",
                "let second = take_cstring (Box (c_string \"x\"))\n",
                "let third = take_nested (Box (Box 2))\n",
                "let fourth = take_template (Box 3)\n",
            ),
        ] {
            let root = standard_library_root();
            let program = ProgramLoader::new()
                .with_standard_library_root(&root)
                .load_source(source, &root)
                .expect("test source should load");
            let resolved = NameResolver::new()
                .resolve_program(program)
                .expect("test source should resolve");
            let typed = TypeChecker::new()
                .check(resolved)
                .expect("test source should type check");
            let module = Lowerer::new().lower(&typed).expect("source should lower");
            let context = LayoutContext::new(module.program());
            let types = signature_types(&module);
            assert!(!types.is_empty(), "the fixture must emit concrete types");
            for value_type in types {
                assert_eq!(
                    context.is_copy(&value_type),
                    typed.is_copy_in_function(&value_type, None),
                    "Copy decision diverges for {value_type:?}",
                );
                assert_eq!(
                    typed.type_needs_drop(&value_type),
                    module.program().concrete_needs_drop(&value_type),
                    "needs-drop decision diverges for {value_type:?}",
                );
                assert_eq!(
                    context.is_io(&value_type),
                    typed.is_io_type(&value_type),
                    "IO decision diverges for {value_type:?}",
                );
                assert_eq!(
                    context.is_reactive(&value_type),
                    typed.is_reactive_type(&value_type),
                    "Reactive decision diverges for {value_type:?}",
                );
                let pointer_runtime = typed.is_coroutine_type(&value_type)
                    || typed.is_task_type(&value_type)
                    || typed.is_scheduler_type(&value_type)
                    || typed.is_tasks_type(&value_type)
                    || typed.is_wait_type(&value_type)
                    || typed.is_resolver_type(&value_type)
                    || typed.is_completion_token_type(&value_type);
                assert_eq!(
                    context.is_pointer_runtime(&value_type),
                    pointer_runtime,
                    "runtime-handle decision diverges for {value_type:?}",
                );
            }
        }
    }
}

/// Strips `Wrapper` wrappers from a place's container type
/// before projecting a product field, shared by the emitter' place pointers.
pub(crate) fn strip_place_wrappers(mut value_type: crate::CheckedType) -> crate::CheckedType {
    loop {
        match value_type {
            ty if ty.handle_kind().is_some() => return ty,
            crate::CheckedType::Wrapper { representation, .. } => value_type = *representation,
            other => return other,
        }
    }
}
