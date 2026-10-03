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
            CheckedType::ParameterProduct(_) => {
                unreachable!("parameter products have no value layout")
            }
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
            CheckedType::Array { .. } => Err(Diagnostic::new(
                Span::Compiler,
                "cannot generate code for an array with an unspecialized length",
            )),
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
            CheckedType::Slice(_) => Ok(self.slice_type().into()),
            CheckedType::Ref(_) => Ok(self.context.ptr_type(AddressSpace::default()).into()),
            CheckedType::Buffer(_) => Ok(self.context.ptr_type(AddressSpace::default()).into()),
            CheckedType::CPointer { .. } => {
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
        let fields = product
            .elements
            .iter()
            .map(|element| self.compile_type(&element.value_type))
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        Ok(self.context.struct_type(&fields, true))
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
        let payload = carrier.array_type(length);
        Ok(self
            .context
            .struct_type(&[self.context.i32_type().into(), payload.into()], false))
    }

    pub(crate) fn slice_type(&self) -> inkwell::types::StructType<'context> {
        self.context.struct_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.size_type.into(),
            ],
            false,
        )
    }

    pub(crate) fn buffer_header_type(
        &self,
        element: BasicTypeEnum<'context>,
    ) -> inkwell::types::StructType<'context> {
        self.context.struct_type(
            &[
                self.size_type.into(),
                self.size_type.into(),
                self.context.i8_type().into(),
                element,
            ],
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
                        types.extend(product.elements.iter().map(|e| e.value_type.clone()))
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

    /// The emission `LayoutContext` must select exactly the representations
    /// and pass modes the checker-based predicates selected: `Copy`, `IO`,
    /// `Reactive`, and the pointer-represented runtime handles. Divergence
    /// would silently change IR, so a fixture sweep locks the two sources
    /// together.
    #[test]
    fn layout_context_agrees_with_checker_predicates() {
        for source in [
            concat!(
                "def identity: <T where Copy T> T -> T = value => value\n",
                "def apply: (I32 -> I32) -> I32 -> I32 = f => value => f value\n",
                "let direct: I32 = identity 1\n",
                "let indirect: I32 = apply identity 1\n",
            ),
            concat!(
                "type Point = wrap (I32, I32)\n",
                "def render: <T where Display T> move T -> String = move value => \"value=$value\"\n",
                "let text: String = render 1\n",
                "let make: () -> ((I32, I32) -> Point) = () => Point\n",
                "let point = make () (1, 2)\n",
                "let debug = \"${(1, 2):?}\"\n",
            ),
            concat!(
                "use std.cinterop.(CString, c_string)\n",
                "use std.coroutine.*\n",
                "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
                "let created = task ()\n",
                "let signal count = 0\n",
                "let doubled = count + count\n",
                "reaction { () }\n",
            ),
            concat!(
                "use std.coroutine.*\n",
                "def observe: move Wait I32 -> Coroutine{} () = move w => coro {\n",
                "    let outcome = await w\n",
                "    match outcome { Completed v => (), Cancelled() => () }\n",
                "}\n",
                "def make: Scheduler -> (wait: Wait I32, resolver: Resolver I32) = s => completion s\n",
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
                "def obs: move Wait () -> Coroutine{} () = move w => coro {\n",
                "    let outcome = await w\n",
                "    match outcome { Completed uu => (), Cancelled() => () }\n",
                "}\n",
                "def mk: Scheduler -> (wait: Wait (), token: CompletionToken) = s => completion_token s\n",
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
                "type Box T = wrap (T)\n",
                "impl<T where Copy T> Drop (Box T) { drop = Box value => () }\n",
                "def take_i32: Box I32 -> I32 = value => 1\n",
                "def take_cstring: Box CString -> I32 = value => 1\n",
                "def take_nested: Box (Box I32) -> I32 = value => 1\n",
                "def take_template: <T where Copy T> Box T -> I32 = value => 1\n",
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
            crate::CheckedType::Wrapper { representation, .. } => value_type = *representation,
            other => return other,
        }
    }
}
