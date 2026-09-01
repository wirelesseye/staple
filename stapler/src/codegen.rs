use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use inkwell::{
    AddressSpace, OptimizationLevel,
    memory_buffer::MemoryBuffer,
    module::Module as LlvmModule,
    targets::TargetData,
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
    },
    types::{BasicType, BasicTypeEnum},
    values::{AnyValue, AnyValueEnum, BasicValue, BasicValueEnum},
};

use crate::typecheck::{
    contains_type_parameter, infer_type_parameters, select_sum_alternative, slice_ref_length,
    substitute_type,
};
use crate::{
    CallExpression, CheckedEffectSet, CheckedFunctionType, CheckedMutation, CheckedProductType,
    CheckedResource, CheckedType, CheckedTypeElement, Diagnostic, Expression, FloatType,
    FunctionId, IntegerBinaryOperation, IntegerCompareOperation, IntegerType, IntrinsicFunction,
    Item, ModuleId, NumericType, Pattern, PatternBindingKind, ProductExpression,
    RepeatedProductExpression, ResolvedFunction,
    ResolvedModule, Span, SymbolId, TypeParameterId, TypedModule,
};

pub struct CodeGenerator<'context> {
    context: &'context inkwell::context::Context,
}

struct ModuleEmitter<'module, 'context> {
    context: &'context inkwell::context::Context,
    typed_module: &'module TypedModule,
    llvm_module: inkwell::module::Module<'context>,
    builder: inkwell::builder::Builder<'context>,
    functions: HashMap<FunctionId, inkwell::values::FunctionValue<'context>>,
    specialized_functions: HashMap<(FunctionId, String), inkwell::values::FunctionValue<'context>>,
    constructor_codes: HashMap<(SymbolId, String), inkwell::values::FunctionValue<'context>>,
    structural_trait_codes:
        HashMap<(crate::StructuralTraitMethod, String), inkwell::values::FunctionValue<'context>>,
    specialization_queue: Vec<(
        FunctionId,
        CheckedFunctionType,
        HashMap<TypeParameterId, CheckedType>,
    )>,
    active_type_substitutions: HashMap<TypeParameterId, CheckedType>,
    expression_type_overrides: HashMap<crate::SyntaxId, CheckedType>,
    function_symbols: HashMap<SymbolId, FunctionId>,
    globals: HashMap<SymbolId, inkwell::values::AnyValueEnum<'context>>,
    closure_codes: HashMap<SymbolId, inkwell::values::FunctionValue<'context>>,
    gc_finalizers: HashMap<String, inkwell::values::FunctionValue<'context>>,
    captured_cell_symbols: HashSet<SymbolId>,
    external_symbols: HashSet<SymbolId>,
    storage: HashMap<SymbolId, inkwell::values::GlobalValue<'context>>,
    initialization_states: HashMap<SymbolId, inkwell::values::GlobalValue<'context>>,
    signal_metadata: HashMap<SymbolId, inkwell::values::GlobalValue<'context>>,
    derived_metadata: HashMap<SymbolId, inkwell::values::GlobalValue<'context>>,
    initializers: HashMap<ModuleId, inkwell::values::FunctionValue<'context>>,
    size_type: inkwell::types::IntType<'context>,
    target_data: TargetData,
}

#[derive(Clone)]
struct SumStorage<'context> {
    tag: inkwell::values::PointerValue<'context>,
    payload: inkwell::values::PointerValue<'context>,
    alignment: u32,
}

struct CompiledCallArguments<'context> {
    values: Vec<inkwell::values::BasicMetadataValueEnum<'context>>,
    temporaries: Vec<(inkwell::values::PointerValue<'context>, CheckedType)>,
}

#[derive(Clone)]
struct BoundResource<'context> {
    resource: CheckedResource,
    value: inkwell::values::AnyValueEnum<'context>,
    indirect: bool,
}

#[derive(Clone, Default)]
struct FunctionEnvironment<'context> {
    locals: HashMap<SymbolId, inkwell::values::AnyValueEnum<'context>>,
    owned: HashMap<
        SymbolId,
        (
            inkwell::values::AnyValueEnum<'context>,
            CheckedType,
            inkwell::values::PointerValue<'context>,
        ),
    >,
    owned_order: Vec<SymbolId>,
    owned_cells: HashSet<SymbolId>,
    binding_cells: HashMap<SymbolId, inkwell::values::PointerValue<'context>>,
    parameter_pointers: HashMap<SymbolId, inkwell::values::PointerValue<'context>>,
    function_id: Option<FunctionId>,
    closure_environment: Option<inkwell::values::PointerValue<'context>>,
    resources: Vec<BoundResource<'context>>,
    reactive_scopes: Vec<inkwell::values::PointerValue<'context>>,
    did_return: bool,
    loops: Vec<LoopCodegenContext<'context>>,
}

impl<'context> FunctionEnvironment<'context> {
    fn restore_local_state(&mut self, snapshot: &Self) {
        self.locals = snapshot.locals.clone();
        self.owned = snapshot.owned.clone();
        self.owned_order = snapshot.owned_order.clone();
        self.owned_cells = snapshot.owned_cells.clone();
        self.binding_cells = snapshot.binding_cells.clone();
        self.parameter_pointers = snapshot.parameter_pointers.clone();
    }
}

#[derive(Clone)]
struct LoopCodegenContext<'context> {
    header: inkwell::basic_block::BasicBlock<'context>,
    exit: inkwell::basic_block::BasicBlock<'context>,
    owned_before: usize,
    reactive_before: usize,
    incoming: Vec<(
        inkwell::values::BasicValueEnum<'context>,
        inkwell::basic_block::BasicBlock<'context>,
    )>,
}

type CodeGenerationResult<T> = Result<T, Diagnostic>;

impl<'module, 'context> ModuleEmitter<'module, 'context> {
    fn build_reactive_runtime_call(
        &self,
        name: &str,
        arguments: &[inkwell::values::BasicMetadataValueEnum<'context>],
        _result: Option<BasicTypeEnum<'context>>,
        call_name: &str,
        span: Span,
    ) -> CodeGenerationResult<Option<BasicValueEnum<'context>>> {
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

    fn new(
        context: &'context inkwell::context::Context,
        typed_module: &'module TypedModule,
        target_machine: &TargetMachine,
    ) -> Self {
        let captured_cell_symbols = typed_module
            .functions()
            .iter()
            .chain(typed_module.implicit_thunks())
            .flat_map(|function| function.captures.iter().copied())
            .filter(|symbol| {
                typed_module.has_mutable_storage(*symbol) || typed_module.is_derived_symbol(*symbol)
            })
            .collect();
        Self {
            context,
            typed_module,
            llvm_module: context.create_module("staple"),
            builder: context.create_builder(),
            functions: HashMap::new(),
            specialized_functions: HashMap::new(),
            constructor_codes: HashMap::new(),
            structural_trait_codes: HashMap::new(),
            specialization_queue: Vec::new(),
            active_type_substitutions: HashMap::new(),
            expression_type_overrides: HashMap::new(),
            function_symbols: HashMap::new(),
            globals: HashMap::new(),
            closure_codes: HashMap::new(),
            gc_finalizers: HashMap::new(),
            captured_cell_symbols,
            external_symbols: HashSet::new(),
            storage: HashMap::new(),
            initialization_states: HashMap::new(),
            signal_metadata: HashMap::new(),
            derived_metadata: HashMap::new(),
            initializers: HashMap::new(),
            size_type: context.ptr_sized_int_type(&target_machine.get_target_data(), None),
            target_data: target_machine.get_target_data(),
        }
    }
}

impl<'context> CodeGenerator<'context> {
    pub fn new(context: &'context inkwell::context::Context) -> Self {
        Self { context }
    }

    pub fn compile_module(&self, module: &TypedModule) -> Result<String, Vec<Diagnostic>> {
        self.compile_module_for_target(module, None)
    }

    pub fn compile_module_for_target(
        &self,
        module: &TypedModule,
        target: Option<&str>,
    ) -> Result<String, Vec<Diagnostic>> {
        let target_machine =
            create_target_machine(target).map_err(|diagnostic| vec![diagnostic])?;
        ModuleEmitter::new(self.context, module, &target_machine)
            .compile(&target_machine)
            .map(|module| module.print_to_string().to_string())
            .map_err(|diagnostic| vec![diagnostic])
    }

    pub fn emit_object(
        &self,
        module: &TypedModule,
        path: &Path,
        target: Option<&str>,
    ) -> Result<(), Vec<Diagnostic>> {
        if path.to_str().is_none() {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "LLVM object output paths must be valid UTF-8",
            )]);
        }
        let target_machine =
            create_target_machine(target).map_err(|diagnostic| vec![diagnostic])?;
        let llvm_module = ModuleEmitter::new(self.context, module, &target_machine)
            .compile(&target_machine)
            .map_err(|diagnostic| vec![diagnostic])?;
        target_machine
            .write_to_file(&llvm_module, FileType::Object, path)
            .map_err(|error| {
                vec![Diagnostic::new(
                    Span::Compiler,
                    format!("could not emit `{}`: {error}", path.display()),
                )]
            })
    }
}

impl<'module, 'context> ModuleEmitter<'module, 'context> {
    fn compile(
        mut self,
        target_machine: &TargetMachine,
    ) -> CodeGenerationResult<LlvmModule<'context>> {
        self.llvm_module.set_triple(&target_machine.get_triple());
        self.llvm_module
            .set_data_layout(&target_machine.get_target_data().get_data_layout());
        self.install_gc_runtime()?;
        self.install_reactive_runtime()?;
        self.declare_external_functions()?;
        self.declare_functions()?;
        self.declare_top_level_storage()?;
        self.declare_initializers();
        self.build_utf8_validator()?;
        let typed_module = self.typed_module;
        let functions = typed_module
            .functions()
            .iter()
            .chain(typed_module.implicit_thunks())
            .cloned()
            .collect::<Vec<_>>();
        for function in &functions {
            let function_type = typed_module
                .type_of_function(function.id)
                .expect("checked function");
            if !contains_type_parameter(&CheckedType::Function(function_type.clone())) {
                self.compile_function_body(function)?;
            }
        }
        self.compile_module_initializers()?;
        self.compile_queued_specializations()?;
        self.compile_main_function()?;

        self.llvm_module.verify().map_err(|message| {
            Diagnostic::new(Span::Compiler, format!("invalid LLVM module: {message}"))
        })?;
        Ok(self.llvm_module)
    }

    fn install_gc_runtime(&self) -> CodeGenerationResult<()> {
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
        let runtime = include_str!("gc.ll")
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

    fn install_reactive_runtime(&self) -> CodeGenerationResult<()> {
        let pointer_bytes = self.target_data.get_pointer_byte_size(None) as u64;
        let bits = pointer_bytes * 8;
        let runtime = include_str!("reactive.ll")
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

    fn declare_external_functions(&mut self) -> CodeGenerationResult<()> {
        for source_module in self.typed_module.resolved().program().modules() {
            for item in &source_module.syntax.items {
                let Item::ExternBlock(block) = item else {
                    continue;
                };
                for binding in &block.bindings {
                    let symbol =
                        self.typed_module
                            .symbol_for(binding.syntax.id)
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    binding.syntax.span.clone(),
                                    "unresolved external binding",
                                )
                            })?;
                    if self
                        .typed_module
                        .resolved()
                        .intrinsic_function(symbol)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(CheckedType::Function(function_type)) =
                        self.typed_module.type_of_symbol(symbol)
                    else {
                        return Err(Diagnostic::new(
                            binding.syntax.span.clone(),
                            "external bindings must have a function type",
                        ));
                    };
                    let llvm_type = self.compile_native_function_type(function_type)?;
                    let function = self
                        .llvm_module
                        .add_function(&binding.name, llvm_type, None);
                    self.globals.insert(symbol, function.into());
                    self.external_symbols.insert(symbol);

                    if !matches!(
                        &*function_type.parameter,
                        CheckedType::Product(product) if product.variadic
                    ) {
                        let adapter_type = self.compile_closure_function_type(function_type)?;
                        let adapter = self.llvm_module.add_function(
                            &format!("__staple_extern_{}", binding.name),
                            adapter_type,
                            Some(inkwell::module::Linkage::Internal),
                        );
                        let entry = self.context.append_basic_block(adapter, "entry");
                        self.builder.position_at_end(entry);
                        let arguments = adapter
                            .get_params()
                            .into_iter()
                            .skip(1)
                            .map(Into::into)
                            .collect::<Vec<_>>();
                        let call = self
                            .builder
                            .build_direct_call(function, &arguments, "extern.call")
                            .map_err(|error| {
                                Diagnostic::new(binding.syntax.span.clone(), error.to_string())
                            })?;
                        let result = call.try_as_basic_value().unwrap_basic();
                        self.builder
                            .build_return(Some(&result))
                            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
                        self.closure_codes.insert(symbol, adapter);
                    }
                }
            }
        }
        Ok(())
    }

    fn declare_top_level_storage(&mut self) -> CodeGenerationResult<()> {
        for source_module in self.typed_module.resolved().program().modules() {
            for item in &source_module.syntax.items {
                if let Item::PatternBinding(binding) = item {
                    self.declare_pattern_storage(source_module.id, &binding.pattern)?;
                    continue;
                }
                let Item::Binding(binding) = item else {
                    continue;
                };
                let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) else {
                    continue;
                };
                self.declare_initialization_state(
                    symbol,
                    &format!("__staple_m{}_{}_state", source_module.id.0, binding.name),
                );
                if !binding.type_parameters.is_empty() {
                    continue;
                }
                if self.globals.contains_key(&symbol) {
                    continue;
                }
                let Some(value_type) = self.typed_module.type_of_symbol(symbol) else {
                    continue;
                };
                let llvm_type = self.compile_type(value_type)?;
                let name = format!("__staple_m{}_{}", source_module.id.0, binding.name);
                let global = self.llvm_module.add_global(llvm_type, None, &name);
                let zero = llvm_type.const_zero();
                global.set_initializer(&zero);
                global.set_linkage(inkwell::module::Linkage::Internal);
                self.storage.insert(symbol, global);
                if self.typed_module.resolved().is_signal_symbol(symbol) {
                    let metadata = self.llvm_module.add_global(
                        self.context.ptr_type(AddressSpace::default()),
                        None,
                        &format!("{name}_signal"),
                    );
                    metadata.set_initializer(
                        &self.context.ptr_type(AddressSpace::default()).const_null(),
                    );
                    metadata.set_linkage(inkwell::module::Linkage::Internal);
                    self.signal_metadata.insert(symbol, metadata);
                } else if self.typed_module.is_derived_symbol(symbol) {
                    let metadata = self.llvm_module.add_global(
                        self.context.ptr_type(AddressSpace::default()),
                        None,
                        &format!("{name}_derived"),
                    );
                    metadata.set_initializer(
                        &self.context.ptr_type(AddressSpace::default()).const_null(),
                    );
                    metadata.set_linkage(inkwell::module::Linkage::Internal);
                    self.derived_metadata.insert(symbol, metadata);
                }
            }
        }
        Ok(())
    }

    fn declare_pattern_storage(
        &mut self,
        module: ModuleId,
        pattern: &Pattern,
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
            Pattern::Binding(binding) => {
                let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) else {
                    return Ok(());
                };
                let Some(value_type) = self.typed_module.type_of_symbol(symbol) else {
                    return Ok(());
                };
                self.declare_initialization_state(
                    symbol,
                    &format!("__staple_m{}_{}_state", module.0, binding.name),
                );
                let llvm_type = self.compile_type(value_type)?;
                let global = self.llvm_module.add_global(
                    llvm_type,
                    None,
                    &format!("__staple_m{}_{}", module.0, binding.name),
                );
                global.set_initializer(&llvm_type.const_zero());
                global.set_linkage(inkwell::module::Linkage::Internal);
                self.storage.insert(symbol, global);
            }
            Pattern::At(at) => {
                self.declare_pattern_storage(
                    module,
                    &Pattern::Binding(at.binding.as_ref().clone()),
                )?;
                self.declare_pattern_storage(module, &at.pattern)?;
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.declare_pattern_storage(module, element)?;
                }
            }
            Pattern::Nominal(pattern) => {
                self.declare_pattern_storage(module, &pattern.argument)?;
            }
        }
        Ok(())
    }

    fn declare_initialization_state(&mut self, symbol: SymbolId, name: &str) {
        if !self
            .typed_module
            .resolved()
            .requires_initialization_state(symbol)
            || self.initialization_states.contains_key(&symbol)
        {
            return;
        }
        let state = self
            .llvm_module
            .add_global(self.context.i8_type(), None, name);
        state.set_initializer(&self.context.i8_type().const_zero());
        state.set_linkage(inkwell::module::Linkage::Internal);
        self.initialization_states.insert(symbol, state);
    }

    fn declare_initializers(&mut self) {
        let function_type = self.context.void_type().fn_type(&[], false);
        for source_module in self.typed_module.resolved().program().modules() {
            let function = self.llvm_module.add_function(
                &format!("__staple_init_m{}", source_module.id.0),
                function_type,
                Some(inkwell::module::Linkage::Internal),
            );
            self.initializers.insert(source_module.id, function);
        }
    }

    fn declare_functions(&mut self) -> CodeGenerationResult<()> {
        let functions = self
            .typed_module
            .functions()
            .iter()
            .chain(self.typed_module.implicit_thunks())
            .cloned()
            .collect::<Vec<_>>();
        for function in &functions {
            let function_type =
                self.typed_module
                    .type_of_function(function.id)
                    .ok_or_else(|| {
                        Diagnostic::new(function.body.syntax().span.clone(), "unchecked function")
                    })?;
            if let Some(binding_syntax) = function.binding_syntax
                && let Some(symbol) = self.typed_module.symbol_for(binding_syntax)
            {
                self.function_symbols.insert(symbol, function.id);
            }
            if contains_type_parameter(&CheckedType::Function(function_type.clone())) {
                continue;
            }
            let llvm_type = self.compile_closure_function_type(function_type)?;
            let llvm_function = self
                .llvm_module
                .add_function(&function.name, llvm_type, None);
            self.functions.insert(function.id, llvm_function);
        }
        Ok(())
    }

    fn compile_function_body(&mut self, function: &ResolvedFunction) -> CodeGenerationResult<()> {
        let llvm_function = self.functions[&function.id];
        self.compile_function_body_as(function, llvm_function)
    }

    fn compile_function_body_as(
        &mut self,
        function: &ResolvedFunction,
        llvm_function: inkwell::values::FunctionValue<'context>,
    ) -> CodeGenerationResult<()> {
        let entry = self.context.append_basic_block(llvm_function, "entry");
        self.builder.position_at_end(entry);

        let mut environment = FunctionEnvironment {
            function_id: Some(function.id),
            ..FunctionEnvironment::default()
        };
        self.bind_function_parameters(&mut environment, function, llvm_function)?;
        if self.typed_module.is_drop_method(function.id) {
            environment.owned.clear();
            environment.owned_order.clear();
        }
        let value = self.compile_expression(&mut environment, &function.body)?;
        if !environment.did_return {
            let return_value = value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    function.body.syntax().span.clone(),
                    "function result is not a first-class value",
                )
            })?;
            self.drop_all_owned(&mut environment, function.body.syntax().span.clone())?;
            self.builder
                .build_return(Some(&return_value))
                .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        }
        Ok(())
    }

    fn specialization_key(function_type: &CheckedFunctionType) -> String {
        format!("{function_type:?}")
    }

    fn ensure_function_specialization(
        &mut self,
        function_id: FunctionId,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let key = Self::specialization_key(function_type);
        if let Some(function) = self
            .specialized_functions
            .get(&(function_id, key.clone()))
            .copied()
        {
            return Ok(function);
        }
        let template = self
            .typed_module
            .type_of_function(function_id)
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "unchecked generic function"))?;
        let mut substitutions = HashMap::new();
        if !infer_type_parameters(
            &CheckedType::Function(template.clone()),
            &CheckedType::Function(function_type.clone()),
            &mut substitutions,
        ) || contains_type_parameter(&CheckedType::Function(function_type.clone()))
        {
            return Err(Diagnostic::new(
                Span::Compiler,
                format!(
                    "generic function use is not fully specialized: {}",
                    CheckedType::Function(function_type.clone())
                ),
            ));
        }
        for value_type in substitutions.values_mut() {
            *value_type = substitute_type(value_type.clone(), &self.active_type_substitutions);
        }
        for (id, value_type) in &self.active_type_substitutions {
            substitutions
                .entry(*id)
                .or_insert_with(|| value_type.clone());
        }
        let source = self
            .typed_module
            .functions()
            .iter()
            .find(|function| function.id == function_id)
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "missing generic function"))?;
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let name = format!("{}__{:016x}", source.name, hasher.finish());
        let llvm_type = self.compile_closure_function_type(function_type)?;
        let llvm_function = self.llvm_module.add_function(
            &name,
            llvm_type,
            Some(inkwell::module::Linkage::Internal),
        );
        self.specialized_functions
            .insert((function_id, key), llvm_function);
        self.specialization_queue
            .push((function_id, function_type.clone(), substitutions));
        Ok(llvm_function)
    }

    fn ensure_constructor_adapter(
        &mut self,
        symbol: SymbolId,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let key = Self::specialization_key(function_type);
        if let Some(function) = self.constructor_codes.get(&(symbol, key.clone())).copied() {
            return Ok(function);
        }
        if contains_type_parameter(&CheckedType::Function(function_type.clone())) {
            return Err(Diagnostic::new(
                Span::Compiler,
                "constructor value is not fully specialized",
            ));
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let name = format!("__staple_constructor_{}_{:016x}", symbol.0, hasher.finish());
        let llvm_type = self.compile_closure_function_type(function_type)?;
        let function = self.llvm_module.add_function(
            &name,
            llvm_type,
            Some(inkwell::module::Linkage::Internal),
        );
        self.constructor_codes.insert((symbol, key), function);

        let previous_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);
        let parameters = function.get_params();
        let value = self.build_product_value(&parameters[1..], Span::Compiler)?;
        let value = if let CheckedType::Ref(payload) = function_type.result.as_ref() {
            self.build_ref_value(value, payload, Span::Compiler)?
                .as_basic_value_enum()
        } else {
            value
        };
        self.builder
            .build_return(Some(&value))
            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        if let Some(block) = previous_block {
            self.builder.position_at_end(block);
        }
        Ok(function)
    }

    fn compile_queued_specializations(&mut self) -> CodeGenerationResult<()> {
        let mut index = 0;
        while index < self.specialization_queue.len() {
            let (function_id, function_type, substitutions) =
                self.specialization_queue[index].clone();
            index += 1;
            let key = Self::specialization_key(&function_type);
            let llvm_function = self.specialized_functions[&(function_id, key)];
            let function = self
                .typed_module
                .functions()
                .iter()
                .find(|function| function.id == function_id)
                .cloned()
                .ok_or_else(|| Diagnostic::new(Span::Compiler, "missing generic function"))?;
            let previous = std::mem::replace(&mut self.active_type_substitutions, substitutions);
            self.compile_function_body_as(&function, llvm_function)?;
            self.active_type_substitutions = previous;
        }
        Ok(())
    }

    fn concrete_expression_type(&self, expression: &Expression) -> Option<CheckedType> {
        let mut value_type = self
            .expression_type_overrides
            .get(&expression.syntax().id)
            .cloned()
            .or_else(|| {
                self.typed_module
                    .type_of_expression(expression.syntax().id)
                    .cloned()
            })
            .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))?;
        let function_id = self
            .typed_module
            .function_for(expression.syntax().id)
            .or_else(|| {
                self.typed_module
                    .symbol_for(expression.syntax().id)
                    .and_then(|symbol| self.function_symbols.get(&symbol).copied())
            });
        if let Some(function_id) = function_id
            && let CheckedType::Function(function) = &mut value_type
            && let Some(template) = self.typed_module.type_of_function(function_id)
        {
            let resources = substitute_type(
                CheckedType::Function(template.clone()),
                &self.active_type_substitutions,
            );
            if let CheckedType::Function(template) = resources
                && !contains_type_parameter(&CheckedType::Function(template.clone()))
            {
                function.effects = template.effects;
            }
        }
        Some(value_type)
    }

    fn bind_function_parameters(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        function: &ResolvedFunction,
        llvm_function: inkwell::values::FunctionValue<'context>,
    ) -> CodeGenerationResult<()> {
        let parameters = llvm_function.get_params();
        let environment_pointer = parameters
            .first()
            .copied()
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "missing closure environment"))?
            .into_pointer_value();
        environment.closure_environment = Some(environment_pointer);
        let function_type = self
            .typed_module
            .type_of_function(function.id)
            .cloned()
            .map(|ty| substitute_type(CheckedType::Function(ty), &self.active_type_substitutions))
            .and_then(|ty| match ty {
                CheckedType::Function(function) => Some(function),
                _ => None,
            })
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "missing checked function type"))?;
        let resource_count = function_type.effects.resources.len();
        for (resource, value) in function_type
            .effects
            .resources
            .iter()
            .cloned()
            .zip(parameters.iter().skip(1).take(resource_count).copied())
        {
            let indirect = resource.mutable
                || !self
                    .typed_module
                    .is_copy_in_function(&resource.value_type, environment.function_id);
            environment.resources.push(BoundResource {
                resource,
                value: value.as_any_value_enum(),
                indirect,
            });
        }
        if !function.captures.is_empty() {
            let environment_type = self.compile_capture_type(function)?;
            let environment_value = self
                .builder
                .build_load(environment_type, environment_pointer, "closure.environment")
                .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?
                .into_struct_value();
            for (index, symbol) in function.captures.iter().copied().enumerate() {
                let value = self
                    .builder
                    .build_extract_value(environment_value, index as u32, "capture")
                    .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
                if self
                    .typed_module
                    .resolved()
                    .requires_initialization_state(symbol)
                    || self.typed_module.has_mutable_storage(symbol)
                    || self.typed_module.is_derived_symbol(symbol)
                {
                    if self.typed_module.is_mutated_parameter(symbol) {
                        environment
                            .parameter_pointers
                            .insert(symbol, value.into_pointer_value());
                    } else {
                        environment
                            .binding_cells
                            .insert(symbol, value.into_pointer_value());
                    }
                } else {
                    environment.locals.insert(symbol, value.as_any_value_enum());
                }
            }
        }
        let raw_parameters = &parameters[1 + resource_count..];
        let whole_mutation = function_type.mutations.contains(&CheckedMutation::Whole);
        let logical_types = flattened_parameter_types(&function_type.parameter);
        let mutation_mask = mutation_parameter_mask(logical_types.len(), &function_type.mutations);
        let mut values = Vec::new();
        let mut mutable_pointers = Vec::new();
        if whole_mutation {
            let pointer = raw_parameters[0].into_pointer_value();
            let llvm_type = self.compile_type(&function_type.parameter)?;
            values.push(
                self.builder
                    .build_load(llvm_type, pointer, "parameter.value")
                    .map_err(compiler_diagnostic)?,
            );
            mutable_pointers.push((0, pointer));
        } else {
            for (index, parameter) in raw_parameters.iter().copied().enumerate() {
                if mutation_mask[index] {
                    let pointer = parameter.into_pointer_value();
                    let llvm_type = self.compile_type(logical_types[index])?;
                    values.push(
                        self.builder
                            .build_load(llvm_type, pointer, "parameter.value")
                            .map_err(compiler_diagnostic)?,
                    );
                    mutable_pointers.push((index, pointer));
                } else {
                    values.push(parameter);
                }
            }
        }
        self.bind_mutable_parameter_pointers(
            environment,
            &function.pattern,
            &function_type.parameter,
            whole_mutation,
            &mutable_pointers,
        )?;
        self.bind_top_level_pattern(environment, &function.pattern, &values)
    }

    fn bind_mutable_parameter_pointers(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        pattern: &Pattern,
        parameter_type: &CheckedType,
        whole: bool,
        pointers: &[(usize, inkwell::values::PointerValue<'context>)],
    ) -> CodeGenerationResult<()> {
        let symbols = top_level_pattern_symbols(self.typed_module.resolved(), pattern);
        if whole {
            let pointer = pointers[0].1;
            if symbols.len() == 1 {
                if let Some(symbol) = symbols[0] {
                    environment.binding_cells.remove(&symbol);
                    environment.parameter_pointers.insert(symbol, pointer);
                }
                return Ok(());
            }
            let CheckedType::Product(product) = parameter_type else {
                return Ok(());
            };
            let llvm_type = self.compile_type(parameter_type)?.into_struct_type();
            for (index, symbol) in symbols.into_iter().enumerate() {
                if let Some(symbol) = symbol {
                    let field = self
                        .builder
                        .build_struct_gep(llvm_type, pointer, index as u32, "parameter.field")
                        .map_err(compiler_diagnostic)?;
                    environment.binding_cells.remove(&symbol);
                    environment.parameter_pointers.insert(symbol, field);
                }
            }
            let _ = product;
            return Ok(());
        }
        for (index, pointer) in pointers {
            if let Some(Some(symbol)) = symbols.get(*index) {
                environment.binding_cells.remove(symbol);
                environment.parameter_pointers.insert(*symbol, *pointer);
            }
        }
        Ok(())
    }

    fn bind_top_level_pattern(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        pattern: &Pattern,
        values: &[BasicValueEnum<'context>],
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Binding(_) | Pattern::At(_) | Pattern::Wildcard(_) | Pattern::Nominal(_) => {
                let value = self.build_product_value(values, pattern.syntax().span.clone())?;
                self.bind_pattern_value(environment, pattern, value)
            }
            Pattern::Product(product) if product.elements.len() == 1 => {
                self.bind_top_level_pattern(environment, &product.elements[0], values)
            }
            Pattern::Product(product) if product.elements.len() == values.len() => {
                for (pattern, value) in product.elements.iter().zip(values.iter().copied()) {
                    self.bind_pattern_value(environment, pattern, value)?;
                }
                Ok(())
            }
            // A whole `mut`/`move`-marked product pattern (`mut (a, b) => ...`):
            // the whole parameter is passed as a single value (by address for
            // `mut`, collapsed via `build_product_value` otherwise), so it
            // must be destructured from that one value rather than zipped
            // element-by-element against a matching `values` slice.
            Pattern::Product(_) if values.len() == 1 => {
                self.bind_pattern_value(environment, pattern, values[0])
            }
            _ => Err(Diagnostic::new(
                pattern.syntax().span.clone(),
                "function pattern layout does not match its declared type",
            )),
        }
    }

    fn bind_pattern_value(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        pattern: &Pattern,
        value: BasicValueEnum<'context>,
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Wildcard(pattern) => {
                let value_type = self
                    .typed_module
                    .type_of_pattern(pattern.syntax.id)
                    .cloned()
                    .map(|ty| substitute_type(ty, &self.active_type_substitutions));
                if let Some(value_type) = value_type
                    && self.typed_module.type_needs_drop(&value_type)
                {
                    self.compile_drop_value(value, &value_type, pattern.syntax.span.clone())?;
                }
                Ok(())
            }
            Pattern::StringLiteral(_) => Ok(()),
            Pattern::Binding(binding) => {
                if self
                    .typed_module
                    .resolved()
                    .type_for_pattern(binding.syntax.id)
                    .is_some()
                {
                    return Ok(());
                }
                value.set_name(&binding.name);
                let symbol = self
                    .typed_module
                    .symbol_for(binding.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "unresolved pattern binding")
                    })?;
                if environment.parameter_pointers.contains_key(&symbol) {
                    environment.locals.insert(symbol, value.as_any_value_enum());
                    return Ok(());
                }
                // `has_mutable_storage` subsumes `binding.mutable` (the
                // resolver's set is built from exactly that flag) and
                // additionally covers a function parameter that a `mut`
                // effect permits writing into — parameter mutability is
                // declared on the signature, not readable from `binding`
                // here.
                if self.typed_module.has_mutable_storage(symbol)
                    && !self.storage.contains_key(&symbol)
                {
                    let cell = self.allocate_binding_cell(
                        environment,
                        symbol,
                        binding.syntax.span.clone(),
                    )?;
                    let cell_type = self.compile_binding_cell_type(symbol)?;
                    let slot = self
                        .builder
                        .build_struct_gep(cell_type, cell, 0, "binding.value")
                        .map_err(compiler_diagnostic)?;
                    self.builder
                        .build_store(slot, value)
                        .map_err(compiler_diagnostic)?;
                    self.store_local_initialization_state(
                        environment,
                        symbol,
                        2,
                        binding.syntax.span.clone(),
                    )?;
                    return Ok(());
                }
                environment.locals.insert(symbol, value.as_any_value_enum());
                self.track_symbol_ownership(environment, symbol)?;
                Ok(())
            }
            Pattern::At(at) => {
                self.bind_pattern_value(
                    environment,
                    &Pattern::Binding(at.binding.as_ref().clone()),
                    value,
                )?;
                self.bind_pattern_value(environment, &at.pattern, value)
            }
            Pattern::Product(product) if product.elements.len() == 1 => {
                self.bind_pattern_value(environment, &product.elements[0], value)
            }
            Pattern::Product(product) if product.elements.is_empty() => Ok(()),
            Pattern::Product(product) => {
                let BasicValueEnum::StructValue(product_value) = value else {
                    return Err(Diagnostic::new(
                        product.syntax.span.clone(),
                        "nested product pattern requires a product value",
                    ));
                };
                for (index, pattern) in product.elements.iter().enumerate() {
                    let element = self
                        .builder
                        .build_extract_value(product_value, index as u32, "pattern.element")
                        .map_err(|error| {
                            Diagnostic::new(product.syntax.span.clone(), error.to_string())
                        })?;
                    self.bind_pattern_value(environment, pattern, element)?;
                }
                Ok(())
            }
            Pattern::Nominal(pattern) => {
                let value = match self
                    .typed_module
                    .type_of_pattern(pattern.syntax.id)
                    .cloned()
                    .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))
                {
                    Some(CheckedType::Ref(payload)) => {
                        self.load_ref_payload(value, &payload, pattern.syntax.span.clone())?
                    }
                    _ => value,
                };
                self.bind_pattern_value(environment, &pattern.argument, value)
            }
            Pattern::Splice(splice) => Err(Diagnostic::new(
                splice.syntax.span.clone(),
                "unexpanded pattern splice",
            )),
        }
    }

    fn track_symbol_ownership(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        symbol: SymbolId,
    ) -> CodeGenerationResult<()> {
        if self.typed_module.is_non_owning_symbol(symbol) {
            return Ok(());
        }
        let Some(value) = environment.locals.get(&symbol).copied() else {
            return Ok(());
        };
        let Some(value_type) = self.typed_module.type_of_symbol(symbol).cloned() else {
            return Ok(());
        };
        let value_type = substitute_type(value_type, &self.active_type_substitutions);
        if !self.typed_module.type_needs_drop(&value_type) {
            return Ok(());
        }
        let live = self
            .builder
            .build_alloca(self.context.bool_type(), "drop.live")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(live, self.context.bool_type().const_int(1, false))
            .map_err(compiler_diagnostic)?;
        environment.owned.insert(symbol, (value, value_type, live));
        if !environment.owned_order.contains(&symbol) {
            environment.owned_order.push(symbol);
        }
        Ok(())
    }

    fn release_moved_ownership(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        syntax: crate::SyntaxId,
    ) -> CodeGenerationResult<()> {
        for symbol in self.typed_module.moved_symbols(syntax) {
            if let Some((_, _, live)) = environment.owned.get(&symbol) {
                self.builder
                    .build_store(*live, self.context.bool_type().const_zero())
                    .map_err(compiler_diagnostic)?;
            }
            if self.typed_module.has_mutable_storage(symbol) {
                self.store_local_initialization_state(environment, symbol, 0, Span::Compiler)?;
            }
        }
        Ok(())
    }

    fn drop_owned_since(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        start: usize,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let symbols = environment.owned_order[start..].to_vec();
        for symbol in symbols.into_iter().rev() {
            if let Some((value, value_type, live)) = environment.owned.remove(&symbol)
                && let Some(value) = value_as_basic(value)
            {
                self.compile_conditional_drop(value, &value_type, live, span.clone())?;
            } else if environment.owned_cells.remove(&symbol)
                && let Some(cell) = environment.binding_cells.get(&symbol).copied()
                && let Some(value_type) = self.typed_module.type_of_symbol(symbol).cloned()
            {
                let value_type = substitute_type(value_type, &self.active_type_substitutions);
                self.compile_conditional_cell_drop(cell, &value_type, span.clone())?;
            }
        }
        environment.owned_order.truncate(start);
        Ok(())
    }

    fn drop_all_owned(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let symbols = environment.owned_order.clone();
        for symbol in symbols.into_iter().rev() {
            if let Some((value, value_type, live)) = environment.owned.get(&symbol).cloned()
                && let Some(value) = value_as_basic(value)
            {
                self.compile_conditional_drop(value, &value_type, live, span.clone())?;
            } else if environment.owned_cells.contains(&symbol)
                && let Some(cell) = environment.binding_cells.get(&symbol).copied()
                && let Some(value_type) = self.typed_module.type_of_symbol(symbol).cloned()
            {
                let value_type = substitute_type(value_type, &self.active_type_substitutions);
                self.compile_conditional_cell_drop(cell, &value_type, span.clone())?;
            }
        }
        Ok(())
    }

    fn compile_drop_value(
        &mut self,
        value: BasicValueEnum<'context>,
        value_type: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<()> {
        if let Some(function_id) = self.typed_module.drop_method_for(value_type) {
            let function = self.functions.get(&function_id).copied().ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing compiled Drop implementation")
            })?;
            let null_environment = self.context.ptr_type(AddressSpace::default()).const_null();
            self.builder
                .build_direct_call(
                    function,
                    &[null_environment.into(), value.into()],
                    "drop.call",
                )
                .map_err(|error| Diagnostic::new(span, error.to_string()))?;
            if let CheckedType::Distinct { representation, .. } = value_type {
                self.compile_drop_value(value, representation, Span::Compiler)?;
            }
            return Ok(());
        }

        match value_type {
            CheckedType::CString => {
                let BasicValueEnum::PointerValue(pointer) = value else {
                    return Err(Diagnostic::new(
                        span,
                        "CString has an invalid representation",
                    ));
                };
                let free_type = self.context.void_type().fn_type(
                    &[self.context.ptr_type(AddressSpace::default()).into()],
                    false,
                );
                let free = self
                    .llvm_module
                    .get_function("free")
                    .unwrap_or_else(|| self.llvm_module.add_function("free", free_type, None));
                self.builder
                    .build_direct_call(free, &[pointer.into()], "c_string.drop")
                    .map_err(|error| Diagnostic::new(span, error.to_string()))?;
            }
            CheckedType::Product(product) => {
                let BasicValueEnum::StructValue(product_value) = value else {
                    return Err(Diagnostic::new(
                        span,
                        "product has an invalid representation",
                    ));
                };
                for (index, element) in product.elements.iter().enumerate().rev() {
                    if !self.typed_module.type_needs_drop(&element.value_type) {
                        continue;
                    }
                    let field = self
                        .builder
                        .build_extract_value(product_value, index as u32, "drop.field")
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                    self.compile_drop_value(field, &element.value_type, span.clone())?;
                }
            }
            CheckedType::Sum(sum) => {
                let BasicValueEnum::StructValue(sum_value) = value else {
                    return Err(Diagnostic::new(span, "sum has an invalid representation"));
                };
                let tag = self
                    .builder
                    .build_extract_value(sum_value, 0, "drop.tag")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                    .into_int_value();
                let function = self
                    .builder
                    .get_insert_block()
                    .and_then(|block| block.get_parent())
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "drop glue is not in a function")
                    })?;
                let merge = self.context.append_basic_block(function, "drop.sum.done");
                let mut cases = Vec::with_capacity(sum.alternatives.len());
                for (index, _) in sum.alternatives.iter().enumerate() {
                    cases.push((
                        self.context.i32_type().const_int(index as u64, false),
                        self.context.append_basic_block(function, "drop.sum.case"),
                    ));
                }
                self.builder
                    .build_switch(tag, merge, &cases)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                for (index, alternative) in sum.alternatives.iter().enumerate() {
                    self.builder.position_at_end(cases[index].1);
                    if self.typed_module.type_needs_drop(alternative) {
                        let payload =
                            self.extract_sum_alternative(sum_value, sum, index, span.clone())?;
                        self.compile_drop_value(payload, alternative, span.clone())?;
                    }
                    self.builder
                        .build_unconditional_branch(merge)
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                }
                self.builder.position_at_end(merge);
            }
            CheckedType::Distinct { representation, .. } => {
                self.compile_drop_value(value, representation, span)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn compile_conditional_drop(
        &mut self,
        value: BasicValueEnum<'context>,
        value_type: &CheckedType,
        live: inkwell::values::PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "drop is not in a function"))?;
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
        self.compile_drop_value(value, value_type, span)?;
        self.builder
            .build_unconditional_branch(done_block)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(done_block);
        Ok(())
    }

    fn compile_main_function(&mut self) -> CodeGenerationResult<()> {
        let integer_type = self.context.i32_type();
        let function_type = integer_type.fn_type(&[], false);
        let function = self.llvm_module.add_function("main", function_type, None);
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);

        let stack_bottom = self
            .builder
            .build_alloca(self.context.i8_type(), "gc.stack.bottom")
            .map_err(compiler_diagnostic)?;
        let set_stack_bottom = self
            .llvm_module
            .get_function("__staple_gc_set_stack_bottom")
            .expect("GC runtime stack initializer");
        self.builder
            .build_direct_call(set_stack_bottom, &[stack_bottom.into()], "")
            .map_err(compiler_diagnostic)?;

        let global_roots = self
            .storage
            .iter()
            .filter_map(|(symbol, global)| {
                self.typed_module
                    .type_of_symbol(*symbol)
                    .filter(|value_type| checked_type_contains_ref(value_type))
                    .map(|value_type| (*global, value_type.clone()))
            })
            .collect::<Vec<_>>();
        for (global, value_type) in global_roots {
            let llvm_type = self.compile_type(&value_type)?;
            self.register_gc_root_region(
                global.as_pointer_value(),
                self.target_data.get_store_size(&llvm_type),
                Span::Compiler,
            )?;
        }

        for module in self
            .typed_module
            .resolved()
            .program()
            .initialization_order()
        {
            self.builder
                .build_call(self.initializers[module], &[], "initialize")
                .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        }
        self.builder
            .build_return(Some(&integer_type.const_zero()))
            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        Ok(())
    }

    fn register_gc_root_region(
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

    fn compile_module_initializers(&mut self) -> CodeGenerationResult<()> {
        let modules = self
            .typed_module
            .resolved()
            .program()
            .modules()
            .iter()
            .map(|module| (module.id, module.syntax.items.clone()))
            .collect::<Vec<_>>();
        let entry_module = self.typed_module.resolved().program().executable_entry();
        for (module_id, items) in modules {
            let function = self.initializers[&module_id];
            let entry = self.context.append_basic_block(function, "entry");
            self.builder.position_at_end(entry);
            let mut environment = FunctionEnvironment::default();
            if Some(module_id) == entry_module
                && let Some(io_resource) = self.typed_module.io_resource()
            {
                let io_type = self.compile_type(&io_resource.value_type)?;
                let io = self
                    .builder
                    .build_alloca(io_type, "io.resource")
                    .map_err(compiler_diagnostic)?;
                self.builder
                    .build_store(io, self.context.struct_type(&[], false).const_zero())
                    .map_err(compiler_diagnostic)?;
                environment.resources.push(BoundResource {
                    resource: io_resource,
                    value: io.as_any_value_enum(),
                    indirect: true,
                });
            }
            let mut entry_reactive_scope_pushed = false;
            if Some(module_id) == entry_module
                && self.typed_module.entry_reactive_required()
                && let Some(reactive_resource) = self.typed_module.reactive_resource()
            {
                let scope = self
                    .build_reactive_runtime_call(
                        "__staple_reactive_scope_create",
                        &[],
                        Some(self.context.ptr_type(AddressSpace::default()).into()),
                        "reactive.scope",
                        Span::Compiler,
                    )?
                    .expect("reactive_scope_create returns a pointer");
                environment.resources.push(BoundResource {
                    resource: reactive_resource,
                    value: scope.as_any_value_enum(),
                    indirect: false,
                });
                environment.reactive_scopes.push(scope.into_pointer_value());
                entry_reactive_scope_pushed = true;
            }
            for item in &items {
                if matches!(
                    item,
                    Item::Binding(_)
                        | Item::PatternBinding(_)
                        | Item::Assignment(_)
                        | Item::Return(_)
                        | Item::Break(_)
                        | Item::Continue(_)
                        | Item::Expression(_)
                ) {
                    self.compile_top_level_item(&mut environment, item)?;
                }
            }
            if entry_reactive_scope_pushed && !environment.did_return {
                self.dispose_reactive_scopes(&environment, 0, Span::Compiler)?;
            }
            self.builder
                .build_return(None)
                .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        }
        Ok(())
    }

    fn compile_top_level_item(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        item: &Item,
    ) -> CodeGenerationResult<()> {
        match item {
            Item::Binding(binding) => {
                let symbol = self
                    .typed_module
                    .symbol_for(binding.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "unresolved binding")
                    })?;
                if !binding.type_parameters.is_empty() {
                    self.store_global_initialization_state(symbol, 1, binding.syntax.span.clone())?;
                    self.store_global_initialization_state(symbol, 2, binding.syntax.span.clone())?;
                    return Ok(());
                }
                let Some(expression) = &binding.value else {
                    return Ok(());
                };
                self.store_global_initialization_state(symbol, 1, binding.syntax.span.clone())?;
                if self.typed_module.is_derived_symbol(symbol) {
                    let global = self.storage.get(&symbol).copied().ok_or_else(|| {
                        Diagnostic::new(
                            binding.syntax.span.clone(),
                            "derived storage is unavailable",
                        )
                    })?;
                    let metadata =
                        self.derived_metadata.get(&symbol).copied().ok_or_else(|| {
                            Diagnostic::new(
                                binding.syntax.span.clone(),
                                "derived metadata is unavailable",
                            )
                        })?;
                    self.compile_derived_create(
                        environment,
                        symbol,
                        global.as_pointer_value(),
                        metadata.as_pointer_value(),
                        binding.syntax.span.clone(),
                    )?;
                    self.store_global_initialization_state(symbol, 2, binding.syntax.span.clone())?;
                    return Ok(());
                }
                let value = self.compile_expression(environment, expression)?;
                if self.typed_module.resolved().is_signal_symbol(symbol)
                    && let Some(metadata) = self.signal_metadata.get(&symbol).copied()
                {
                    let signal = self
                        .build_reactive_runtime_call(
                            "__staple_signal_create",
                            &[],
                            Some(self.context.ptr_type(AddressSpace::default()).into()),
                            "signal.create",
                            binding.syntax.span.clone(),
                        )?
                        .expect("signal_create returns a pointer");
                    self.builder
                        .build_store(metadata.as_pointer_value(), signal)
                        .map_err(compiler_diagnostic)?;
                }
                if let Some(global) = self.storage.get(&symbol) {
                    let value = value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(
                            binding.syntax.span.clone(),
                            "top-level value is not storable",
                        )
                    })?;
                    self.builder
                        .build_store(global.as_pointer_value(), value)
                        .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
                }
                self.store_global_initialization_state(symbol, 2, binding.syntax.span.clone())?;
                Ok(())
            }
            Item::PatternBinding(binding) => {
                self.store_pattern_initialization_state(&binding.pattern, 1)?;
                let value = self.compile_expression(environment, &binding.value)?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        binding.syntax.span.clone(),
                        "destructured value is not first-class",
                    )
                })?;
                self.bind_pattern_value(environment, &binding.pattern, value)?;
                self.store_pattern_globals(environment, &binding.pattern)?;
                self.store_pattern_initialization_state(&binding.pattern, 2)
            }
            Item::Assignment(assignment) => self.compile_assignment(environment, assignment),
            Item::Return(item) => Err(Diagnostic::new(
                item.syntax.span.clone(),
                "`return` is only allowed inside a function",
            )),
            Item::Break(item) => Err(Diagnostic::new(
                item.syntax.span.clone(),
                "`break` is only allowed inside a loop",
            )),
            Item::Continue(item) => Err(Diagnostic::new(
                item.syntax.span.clone(),
                "`continue` is only allowed inside a loop",
            )),
            Item::Expression(expression) => {
                let value = self.compile_expression(environment, expression)?;
                let value_type = self
                    .concrete_expression_type(expression)
                    .unwrap_or(CheckedType::Error);
                if self.typed_module.type_needs_drop(&value_type)
                    && let Some(value) = value_as_basic(value)
                {
                    self.compile_drop_value(value, &value_type, expression.syntax().span.clone())?;
                }
                Ok(())
            }
            Item::Submodule(_) => Ok(()),
            Item::TypeDeclaration(_) => Ok(()),
            Item::UseDeclaration(_) => Ok(()),
            _ => Ok(()),
        }
    }

    fn store_pattern_globals(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        pattern: &Pattern,
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
            Pattern::Binding(binding) => {
                let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) else {
                    return Ok(());
                };
                let Some(global) = self.storage.get(&symbol) else {
                    return Ok(());
                };
                let value = environment.locals.get(&symbol).copied().ok_or_else(|| {
                    Diagnostic::new(binding.syntax.span.clone(), "unbound destructuring pattern")
                })?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(binding.syntax.span.clone(), "pattern value is not storable")
                })?;
                self.builder
                    .build_store(global.as_pointer_value(), value)
                    .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
            }
            Pattern::At(at) => {
                self.store_pattern_globals(
                    environment,
                    &Pattern::Binding(at.binding.as_ref().clone()),
                )?;
                self.store_pattern_globals(environment, &at.pattern)?;
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.store_pattern_globals(environment, element)?;
                }
            }
            Pattern::Nominal(pattern) => {
                self.store_pattern_globals(environment, &pattern.argument)?;
            }
        }
        Ok(())
    }

    fn store_pattern_initialization_state(
        &mut self,
        pattern: &Pattern,
        state: u64,
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => Ok(()),
            Pattern::Binding(binding) => {
                if let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) {
                    self.store_global_initialization_state(
                        symbol,
                        state,
                        binding.syntax.span.clone(),
                    )?;
                }
                Ok(())
            }
            Pattern::At(at) => {
                self.store_pattern_initialization_state(
                    &Pattern::Binding(at.binding.as_ref().clone()),
                    state,
                )?;
                self.store_pattern_initialization_state(&at.pattern, state)
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.store_pattern_initialization_state(element, state)?;
                }
                Ok(())
            }
            Pattern::Nominal(pattern) => {
                self.store_pattern_initialization_state(&pattern.argument, state)
            }
        }
    }

    fn store_global_initialization_state(
        &mut self,
        symbol: SymbolId,
        state: u64,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let Some(slot) = self.initialization_states.get(&symbol) else {
            return Ok(());
        };
        self.builder
            .build_store(
                slot.as_pointer_value(),
                self.context.i8_type().const_int(state, false),
            )
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(())
    }

    fn predeclare_checked_bindings(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        items: &[Item],
    ) -> CodeGenerationResult<()> {
        for item in items {
            let Item::Binding(binding) = item else {
                continue;
            };
            if binding.kind != crate::BindingKind::Def {
                continue;
            }
            let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) else {
                continue;
            };
            if !self
                .typed_module
                .resolved()
                .requires_initialization_state(symbol)
                || environment.binding_cells.contains_key(&symbol)
            {
                continue;
            }
            let generic = self
                .typed_module
                .type_of_symbol(symbol)
                .is_some_and(contains_type_parameter);
            let (cell, state_slot) = if generic {
                let state = self
                    .builder
                    .build_malloc(self.context.i8_type(), "binding.state.cell")
                    .map_err(|error| {
                        Diagnostic::new(binding.syntax.span.clone(), error.to_string())
                    })?;
                self.register_gc_root_region(state, 1, binding.syntax.span.clone())?;
                (state, state)
            } else {
                let cell_type = self.compile_binding_cell_type(symbol)?;
                let cell = self
                    .builder
                    .build_malloc(cell_type, "binding.cell")
                    .map_err(|error| {
                        Diagnostic::new(binding.syntax.span.clone(), error.to_string())
                    })?;
                self.register_gc_root_region(
                    cell,
                    self.target_data.get_store_size(&cell_type),
                    binding.syntax.span.clone(),
                )?;
                let state = self
                    .builder
                    .build_struct_gep(cell_type, cell, 1, "binding.state")
                    .map_err(|error| {
                        Diagnostic::new(binding.syntax.span.clone(), error.to_string())
                    })?;
                (cell, state)
            };
            self.builder
                .build_store(state_slot, self.context.i8_type().const_zero())
                .map_err(|error| Diagnostic::new(binding.syntax.span.clone(), error.to_string()))?;
            environment.binding_cells.insert(symbol, cell);
        }
        Ok(())
    }

    fn compile_binding_cell_type(
        &self,
        symbol: SymbolId,
    ) -> CodeGenerationResult<inkwell::types::StructType<'context>> {
        let value_type = self
            .typed_module
            .type_of_symbol(symbol)
            .cloned()
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "unchecked binding cell"))?;
        let value_type = substitute_type(value_type, &self.active_type_substitutions);
        let llvm_type = self.compile_type(&value_type)?;
        let mut fields = vec![llvm_type, self.context.i8_type().into()];
        if self.typed_module.resolved().is_signal_symbol(symbol)
            || self.typed_module.is_derived_symbol(symbol)
        {
            fields.push(self.context.ptr_type(AddressSpace::default()).into());
        }
        Ok(self.context.struct_type(&fields, false))
    }

    fn allocate_binding_cell(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            return Ok(cell);
        }
        let cell_type = self.compile_binding_cell_type(symbol)?;
        let captured = self.captured_cell_symbols.contains(&symbol);
        let cell = if captured {
            self.build_gc_allocation(
                self.size_type
                    .const_int(self.target_data.get_store_size(&cell_type), false),
                "binding.cell",
                span.clone(),
            )?
        } else {
            self.builder
                .build_alloca(cell_type, "binding.cell")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        };
        let state = self
            .builder
            .build_struct_gep(cell_type, cell, 1, "binding.state")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_store(state, self.context.i8_type().const_zero())
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        if self.typed_module.resolved().is_signal_symbol(symbol) {
            let metadata = self
                .builder
                .build_struct_gep(cell_type, cell, 2, "signal.metadata")
                .map_err(compiler_diagnostic)?;
            let signal = self
                .build_reactive_runtime_call(
                    "__staple_signal_create",
                    &[],
                    Some(self.context.ptr_type(AddressSpace::default()).into()),
                    "signal.create",
                    Span::Compiler,
                )?
                .expect("signal_create returns a pointer");
            self.builder
                .build_store(metadata, signal)
                .map_err(compiler_diagnostic)?;
        }
        let value_type = self
            .typed_module
            .type_of_symbol(symbol)
            .cloned()
            .map(|ty| substitute_type(ty, &self.active_type_substitutions))
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "unchecked binding cell"))?;
        if self.typed_module.type_needs_drop(&value_type) {
            if captured {
                let finalizer = self.ensure_cell_finalizer(&value_type)?;
                self.set_gc_finalizer(cell, finalizer)?;
            } else {
                environment.owned_cells.insert(symbol);
                if !environment.owned_order.contains(&symbol) {
                    environment.owned_order.push(symbol);
                }
            }
        }
        environment.binding_cells.insert(symbol, cell);
        Ok(cell)
    }

    fn store_local_initialization_state(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        state: u64,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let Some(cell) = environment.binding_cells.get(&symbol).copied() else {
            return Ok(());
        };
        let generic = self
            .typed_module
            .type_of_symbol(symbol)
            .cloned()
            .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))
            .is_some_and(|value_type| contains_type_parameter(&value_type));
        let state_slot = if generic {
            cell
        } else {
            let cell_type = self.compile_binding_cell_type(symbol)?;
            self.builder
                .build_struct_gep(cell_type, cell, 1, "binding.state")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        };
        self.builder
            .build_store(state_slot, self.context.i8_type().const_int(state, false))
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        Ok(())
    }

    fn compile_item(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        item: &Item,
    ) -> CodeGenerationResult<Option<AnyValueEnum<'context>>> {
        match item {
            Item::Binding(binding) => {
                if !binding.type_parameters.is_empty() {
                    if let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) {
                        self.store_local_initialization_state(
                            environment,
                            symbol,
                            1,
                            binding.syntax.span.clone(),
                        )?;
                        self.store_local_initialization_state(
                            environment,
                            symbol,
                            2,
                            binding.syntax.span.clone(),
                        )?;
                    }
                    return Ok(None);
                }
                let symbol = self
                    .typed_module
                    .symbol_for(binding.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "unresolved binding")
                    })?;
                if self.typed_module.has_mutable_storage(symbol)
                    || self.typed_module.is_derived_symbol(symbol)
                {
                    self.allocate_binding_cell(environment, symbol, binding.syntax.span.clone())?;
                }
                if environment.binding_cells.contains_key(&symbol) {
                    self.store_local_initialization_state(
                        environment,
                        symbol,
                        1,
                        binding.syntax.span.clone(),
                    )?;
                }
                if let Some(expression) = &binding.value {
                    if self.typed_module.is_derived_symbol(symbol) {
                        let cell =
                            environment
                                .binding_cells
                                .get(&symbol)
                                .copied()
                                .ok_or_else(|| {
                                    Diagnostic::new(
                                        binding.syntax.span.clone(),
                                        "derived cell is unavailable",
                                    )
                                })?;
                        let cell_type = self.compile_binding_cell_type(symbol)?;
                        let value_slot = self
                            .builder
                            .build_struct_gep(cell_type, cell, 0, "derived.value")
                            .map_err(compiler_diagnostic)?;
                        let metadata_slot = self
                            .builder
                            .build_struct_gep(cell_type, cell, 2, "derived.metadata")
                            .map_err(compiler_diagnostic)?;
                        self.compile_derived_create(
                            environment,
                            symbol,
                            value_slot,
                            metadata_slot,
                            binding.syntax.span.clone(),
                        )?;
                        self.store_local_initialization_state(
                            environment,
                            symbol,
                            2,
                            binding.syntax.span.clone(),
                        )?;
                        return Ok(None);
                    }
                    let value = self.compile_expression(environment, expression)?;
                    if environment.did_return {
                        return Ok(None);
                    }
                    if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
                        let value = value_as_basic(value).ok_or_else(|| {
                            Diagnostic::new(
                                binding.syntax.span.clone(),
                                "binding value is not storable",
                            )
                        })?;
                        let cell_type = self.compile_binding_cell_type(symbol)?;
                        let slot = self
                            .builder
                            .build_struct_gep(cell_type, cell, 0, "binding.value")
                            .map_err(|error| {
                                Diagnostic::new(binding.syntax.span.clone(), error.to_string())
                            })?;
                        self.builder.build_store(slot, value).map_err(|error| {
                            Diagnostic::new(binding.syntax.span.clone(), error.to_string())
                        })?;
                        self.store_local_initialization_state(
                            environment,
                            symbol,
                            2,
                            binding.syntax.span.clone(),
                        )?;
                    } else {
                        environment.locals.insert(symbol, value);
                        self.track_symbol_ownership(environment, symbol)?;
                    }
                }
                Ok(None)
            }
            Item::PatternBinding(binding) => {
                let value = self.compile_expression(environment, &binding.value)?;
                if environment.did_return {
                    return Ok(None);
                }
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        binding.syntax.span.clone(),
                        "destructured value is not first-class",
                    )
                })?;
                if binding.kind == PatternBindingKind::Propagating {
                    self.compile_propagating_binding(environment, binding, value)?;
                } else {
                    self.bind_pattern_value(environment, &binding.pattern, value)?;
                }
                Ok(None)
            }
            Item::Assignment(assignment) => {
                self.compile_assignment(environment, assignment)?;
                Ok(None)
            }
            Item::Return(item) => {
                let value = self.compile_expression(environment, &item.value)?;
                if environment.did_return {
                    return Ok(None);
                }
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        item.value.syntax().span.clone(),
                        "function result is not a first-class value",
                    )
                })?;
                self.dispose_reactive_scopes(environment, 0, item.syntax.span.clone())?;
                self.drop_all_owned(environment, item.syntax.span.clone())?;
                self.builder
                    .build_return(Some(&value))
                    .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
                environment.did_return = true;
                Ok(None)
            }
            Item::Break(item) => {
                let value = if let Some(expression) = &item.value {
                    let value = self.compile_expression(environment, expression)?;
                    if environment.did_return {
                        return Ok(None);
                    }
                    value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(
                            expression.syntax().span.clone(),
                            "loop result is not a first-class value",
                        )
                    })?
                } else {
                    value_as_basic(self.unit_value()).expect("unit is a basic value")
                };
                let (exit, owned_before, reactive_before) = environment
                    .loops
                    .last()
                    .map(|loop_| (loop_.exit, loop_.owned_before, loop_.reactive_before))
                    .expect("break inside loop");
                self.dispose_reactive_scopes(
                    environment,
                    reactive_before,
                    item.syntax.span.clone(),
                )?;
                self.drop_owned_since(environment, owned_before, item.syntax.span.clone())?;
                self.builder
                    .build_unconditional_branch(exit)
                    .map_err(|error| {
                        Diagnostic::new(item.syntax.span.clone(), error.to_string())
                    })?;
                let predecessor = self.builder.get_insert_block().expect("break block");
                environment
                    .loops
                    .last_mut()
                    .expect("break inside loop")
                    .incoming
                    .push((value, predecessor));
                environment.did_return = true;
                Ok(None)
            }
            Item::Continue(item) => {
                let (header, owned_before, reactive_before) = environment
                    .loops
                    .last()
                    .map(|loop_| (loop_.header, loop_.owned_before, loop_.reactive_before))
                    .expect("continue inside loop");
                self.dispose_reactive_scopes(
                    environment,
                    reactive_before,
                    item.syntax.span.clone(),
                )?;
                self.drop_owned_since(environment, owned_before, item.syntax.span.clone())?;
                self.builder
                    .build_unconditional_branch(header)
                    .map_err(|error| {
                        Diagnostic::new(item.syntax.span.clone(), error.to_string())
                    })?;
                environment.did_return = true;
                Ok(None)
            }
            Item::Expression(expression) => {
                let value = self.compile_expression(environment, expression)?;
                if !environment.did_return {
                    let value_type = self
                        .concrete_expression_type(expression)
                        .unwrap_or(CheckedType::Error);
                    if self.typed_module.type_needs_drop(&value_type)
                        && let Some(value) = value_as_basic(value)
                    {
                        self.compile_drop_value(
                            value,
                            &value_type,
                            expression.syntax().span.clone(),
                        )?;
                    }
                }
                Ok(Some(value))
            }
            Item::Submodule(_) => Ok(None),
            Item::TypeDeclaration(_) => Ok(None),
            Item::UseDeclaration(_) => Ok(None),
            _ => Ok(None),
        }
    }

    fn compile_assignment(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        assignment: &crate::Assignment,
    ) -> CodeGenerationResult<()> {
        if let Expression::Index(index) = &assignment.target {
            return self.compile_mutate_index_assignment(environment, assignment, index);
        }
        let (pointer, value_type, symbol) =
            self.compile_place_pointer(environment, &assignment.target)?;
        let value = self.compile_expression(environment, &assignment.value)?;
        if environment.did_return {
            return Ok(());
        }
        let value = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(
                assignment.value.syntax().span.clone(),
                "assigned value is not storable",
            )
        })?;

        if self.typed_module.type_needs_drop(&value_type) {
            if let Some(symbol) = symbol
                && let Some(cell) = environment.binding_cells.get(&symbol).copied()
            {
                self.compile_conditional_cell_drop(
                    cell,
                    &value_type,
                    assignment.syntax.span.clone(),
                )?;
            } else {
                let llvm_type = self.compile_type(&value_type)?;
                let old = self
                    .builder
                    .build_load(llvm_type, pointer, "assignment.old")
                    .map_err(compiler_diagnostic)?;
                self.compile_drop_value(old, &value_type, assignment.syntax.span.clone())?;
            }
        }
        self.builder
            .build_store(pointer, value)
            .map_err(|error| Diagnostic::new(assignment.syntax.span.clone(), error.to_string()))?;
        if let Some(symbol) = symbol {
            self.store_local_initialization_state(
                environment,
                symbol,
                2,
                assignment.syntax.span.clone(),
            )?;
            self.store_global_initialization_state(symbol, 2, assignment.syntax.span.clone())?;
            if let Some(signal) =
                self.signal_metadata_value(environment, symbol, assignment.syntax.span.clone())?
            {
                self.build_reactive_runtime_call(
                    "__staple_signal_notify",
                    &[signal.into()],
                    None,
                    "signal.notify",
                    assignment.syntax.span.clone(),
                )?;
            }
        }
        Ok(())
    }

    fn compile_mutate_index_assignment(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        assignment: &crate::Assignment,
        index: &crate::IndexExpression,
    ) -> CodeGenerationResult<()> {
        let dispatch = self
            .typed_module
            .trait_dispatch_for(assignment.syntax.id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(
                    assignment.syntax.span.clone(),
                    "missing MutateIndex dispatch",
                )
            })?;
        let trait_id = self
            .typed_module
            .resolved()
            .trait_for_method(dispatch.method)
            .expect("MutateIndex method owner");
        let arguments = dispatch
            .arguments
            .into_iter()
            .map(|argument| substitute_type(argument, &self.active_type_substitutions))
            .collect::<Vec<_>>();
        let arguments = self
            .typed_module
            .complete_trait_arguments(trait_id, &arguments)
            .ok_or_else(|| {
                Diagnostic::new(
                    assignment.syntax.span.clone(),
                    "incomplete MutateIndex dispatch",
                )
            })?;
        let function = self.trait_method_code(
            trait_id,
            &arguments,
            dispatch.method,
            assignment.syntax.span.clone(),
        )?;
        let function_type = self
            .typed_module
            .instantiated_trait_method_type(trait_id, &arguments, dispatch.method)
            .ok_or_else(|| {
                Diagnostic::new(assignment.syntax.span.clone(), "unchecked MutateIndex call")
            })?;
        let CheckedType::Product(parameter) = function_type.parameter.as_ref() else {
            return Err(Diagnostic::new(
                assignment.syntax.span.clone(),
                "invalid MutateIndex signature",
            ));
        };
        let target_type = &parameter.elements[0].value_type;
        let (target, target_temporary) =
            self.compile_mutation_argument_pointer(environment, &index.value, target_type)?;
        let position = self.compile_expression(environment, &index.index)?;
        if environment.did_return {
            return Ok(());
        }
        let replacement = self.compile_expression(environment, &assignment.value)?;
        if environment.did_return {
            return Ok(());
        }
        let mut call_arguments = self.compile_resource_arguments(
            environment,
            &function_type.effects,
            assignment.syntax.span.clone(),
        )?;
        call_arguments.insert(
            0,
            self.context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
        );
        call_arguments.push(target.into());
        for (value, span) in [
            (position, index.index.syntax().span.clone()),
            (replacement, assignment.value.syntax().span.clone()),
        ] {
            call_arguments.push(
                value_as_basic(value)
                    .ok_or_else(|| {
                        Diagnostic::new(span, "MutateIndex argument is not first-class")
                    })?
                    .into(),
            );
        }
        self.builder
            .build_direct_call(function, &call_arguments, "mutate_index.call")
            .map_err(|error| Diagnostic::new(assignment.syntax.span.clone(), error.to_string()))?;
        self.drop_mutation_temporaries(
            target_temporary.into_iter().collect(),
            assignment.syntax.span.clone(),
        )?;
        Ok(())
    }

    fn compile_place_pointer(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        expression: &Expression,
    ) -> CodeGenerationResult<(
        inkwell::values::PointerValue<'context>,
        CheckedType,
        Option<SymbolId>,
    )> {
        if let Some(symbol) = self.typed_module.symbol_for(expression.syntax().id) {
            let value_type = self
                .typed_module
                .type_of_symbol(symbol)
                .cloned()
                .map(|ty| substitute_type(ty, &self.active_type_substitutions))
                .ok_or_else(|| {
                    Diagnostic::new(expression.syntax().span.clone(), "unchecked place")
                })?;
            if let Some(pointer) = environment.parameter_pointers.get(&symbol).copied() {
                return Ok((pointer, value_type, Some(symbol)));
            }
            if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
                let cell_type = self.compile_binding_cell_type(symbol)?;
                let slot = self
                    .builder
                    .build_struct_gep(cell_type, cell, 0, "binding.value")
                    .map_err(compiler_diagnostic)?;
                return Ok((slot, value_type, Some(symbol)));
            }
            if let Some(global) = self.storage.get(&symbol).copied() {
                return Ok((global.as_pointer_value(), value_type, Some(symbol)));
            }
            return Err(Diagnostic::new(
                expression.syntax().span.clone(),
                "binding storage is not available",
            ));
        }

        match expression {
            Expression::Product(product) if product.elements.len() == 1 => {
                self.compile_place_pointer(environment, &product.elements[0].value)
            }
            Expression::Satisfies(value) => self.compile_place_pointer(environment, &value.value),
            Expression::Resource(resource) => {
                let expected = self
                    .typed_module
                    .resource_for_expression(resource.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(resource.syntax.span.clone(), "unchecked resource place")
                    })?;
                let bound = environment
                    .resources
                    .iter()
                    .rev()
                    .find(|candidate| candidate.resource.value_type == expected.value_type)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            resource.syntax.span.clone(),
                            format!("resource `{}` is not available", expected.value_type),
                        )
                    })?;
                if !bound.indirect {
                    return Err(Diagnostic::new(
                        resource.syntax.span.clone(),
                        format!("resource `{}` is not mutable", expected.value_type),
                    ));
                }
                let pointer = value_as_basic(bound.value)
                    .expect("resource address is first-class")
                    .into_pointer_value();
                Ok((pointer, expected.value_type.clone(), None))
            }
            Expression::Access(access) => {
                let checked = self
                    .typed_module
                    .access_for(access.syntax.id)
                    .cloned()
                    .ok_or_else(|| {
                        Diagnostic::new(access.syntax.span.clone(), "missing checked access")
                    })?;
                let result_type = self.concrete_expression_type(expression).ok_or_else(|| {
                    Diagnostic::new(access.syntax.span.clone(), "unchecked access place")
                })?;
                if let crate::CheckedAccess::Representation { dereference } = &checked {
                    if dereference.is_some() {
                        let reference = self.compile_expression(environment, &access.value)?;
                        let Some(BasicValueEnum::PointerValue(pointer)) = value_as_basic(reference)
                        else {
                            return Err(Diagnostic::new(
                                access.syntax.span.clone(),
                                "invalid Ref place",
                            ));
                        };
                        return Ok((pointer, result_type, None));
                    }
                    let (pointer, _, symbol) =
                        self.compile_place_pointer(environment, &access.value)?;
                    return Ok((pointer, result_type, symbol));
                }
                let crate::CheckedAccess::Product {
                    index,
                    dereference,
                    erased,
                    scalar,
                } = checked
                else {
                    unreachable!("representation access handled above")
                };
                if scalar {
                    let (pointer, _, symbol) =
                        self.compile_place_pointer(environment, &access.value)?;
                    return Ok((pointer, result_type, symbol));
                }
                if erased {
                    let reference = self.compile_expression(environment, &access.value)?;
                    let Some(BasicValueEnum::StructValue(reference)) = value_as_basic(reference)
                    else {
                        return Err(Diagnostic::new(
                            access.syntax.span.clone(),
                            "invalid erased Ref place",
                        ));
                    };
                    let pointer = self
                        .builder
                        .build_extract_value(reference, 0, "place.pointer")
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value();
                    let length = self
                        .builder
                        .build_extract_value(reference, 1, "place.length")
                        .map_err(compiler_diagnostic)?
                        .into_int_value();
                    let position = self.size_type.const_int(index as u64, false);
                    let out = self
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::UGE,
                            position,
                            length,
                            "place.out_of_bounds",
                        )
                        .map_err(compiler_diagnostic)?;
                    self.build_trap_if(out, access.syntax.span.clone())?;
                    let element_type = self.compile_type(&result_type)?;
                    let pointer = unsafe {
                        self.builder
                            .build_gep(element_type, pointer, &[position], "place.element")
                    }
                    .map_err(compiler_diagnostic)?;
                    return Ok((pointer, result_type, None));
                }

                let (pointer, container_type) = if let Some(payload) = dereference {
                    let reference = self.compile_expression(environment, &access.value)?;
                    let Some(BasicValueEnum::PointerValue(pointer)) = value_as_basic(reference)
                    else {
                        return Err(Diagnostic::new(
                            access.syntax.span.clone(),
                            "invalid Ref place",
                        ));
                    };
                    (pointer, payload)
                } else {
                    if let Some(symbol) = self.typed_module.symbol_for(access.value.syntax().id)
                        && self.typed_module.has_mutable_storage(symbol)
                    {
                        self.check_symbol_initialization(
                            environment,
                            symbol,
                            access.value.syntax().span.clone(),
                        )?;
                    }
                    let (pointer, value_type, _) =
                        self.compile_place_pointer(environment, &access.value)?;
                    (pointer, value_type)
                };
                let container_type = strip_place_wrappers(container_type);
                let BasicTypeEnum::StructType(container_llvm) =
                    self.compile_type(&container_type)?
                else {
                    return Err(Diagnostic::new(
                        access.syntax.span.clone(),
                        "access place is not a product",
                    ));
                };
                let pointer = self
                    .builder
                    .build_struct_gep(container_llvm, pointer, index as u32, "place.field")
                    .map_err(compiler_diagnostic)?;
                Ok((pointer, result_type, None))
            }
            Expression::Index(index) => Err(Diagnostic::new(
                index.syntax.span.clone(),
                "indexed values are updated through `MutateIndex`, not as places",
            )),
            _ => Err(Diagnostic::new(
                expression.syntax().span.clone(),
                "assignment target is not a place",
            )),
        }
    }

    fn compile_propagating_binding(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        binding: &crate::PatternBinding,
        value: BasicValueEnum<'context>,
    ) -> CodeGenerationResult<()> {
        let propagation = self
            .typed_module
            .propagation_for(binding.syntax.id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(binding.syntax.span.clone(), "missing checked propagation")
            })?;
        let source = substitute_type(propagation.source, &self.active_type_substitutions);
        let result = substitute_type(propagation.result, &self.active_type_substitutions);
        let CheckedType::Sum(source_sum) = &source else {
            return Err(Diagnostic::new(
                binding.syntax.span.clone(),
                "propagation source is not a sum",
            ));
        };
        let BasicValueEnum::StructValue(sum_value) = value else {
            return Err(Diagnostic::new(
                binding.syntax.span.clone(),
                "propagation source has an invalid representation",
            ));
        };
        let tag = self
            .builder
            .build_extract_value(sum_value, 0, "propagate.tag")
            .map_err(|error| Diagnostic::new(binding.syntax.span.clone(), error.to_string()))?
            .into_int_value();
        let success = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                tag,
                self.context
                    .i32_type()
                    .const_int(propagation.success_index as u64, false),
                "propagate.success",
            )
            .map_err(|error| Diagnostic::new(binding.syntax.span.clone(), error.to_string()))?;
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| {
                Diagnostic::new(
                    binding.syntax.span.clone(),
                    "propagation is not inside a function",
                )
            })?;
        let success_block = self.context.append_basic_block(function, "propagate.ok");
        let failure_block = self
            .context
            .append_basic_block(function, "propagate.return");
        self.builder
            .build_conditional_branch(success, success_block, failure_block)
            .map_err(|error| Diagnostic::new(binding.syntax.span.clone(), error.to_string()))?;

        self.builder.position_at_end(failure_block);
        let failure_value = if source == result {
            sum_value.as_any_value_enum()
        } else if let CheckedType::Sum(_) = result {
            self.coerce_sum_value(
                sum_value.as_any_value_enum(),
                &source,
                &result,
                binding.syntax.span.clone(),
            )?
        } else {
            let index = source_sum
                .alternatives
                .iter()
                .position(|alternative| alternative == &result)
                .ok_or_else(|| {
                    Diagnostic::new(
                        binding.syntax.span.clone(),
                        "propagated result is missing its residual variant",
                    )
                })?;
            self.extract_sum_alternative(sum_value, source_sum, index, binding.syntax.span.clone())?
                .as_any_value_enum()
        };
        let failure_value = value_as_basic(failure_value).ok_or_else(|| {
            Diagnostic::new(
                binding.syntax.span.clone(),
                "propagated result is not first-class",
            )
        })?;
        self.drop_all_owned(environment, binding.syntax.span.clone())?;
        self.builder
            .build_return(Some(&failure_value))
            .map_err(|error| Diagnostic::new(binding.syntax.span.clone(), error.to_string()))?;

        self.builder.position_at_end(success_block);
        let success_value = self.extract_sum_alternative(
            sum_value,
            source_sum,
            propagation.success_index,
            binding.syntax.span.clone(),
        )?;
        let mut root = &binding.pattern;
        while let Pattern::At(at) = root {
            self.bind_pattern_value(
                environment,
                &Pattern::Binding(at.binding.as_ref().clone()),
                sum_value.as_basic_value_enum(),
            )?;
            root = &at.pattern;
        }
        let Pattern::Nominal(pattern) = root else {
            unreachable!("checked propagating pattern is nominal");
        };
        self.bind_pattern_value(environment, &pattern.argument, success_value)
    }

    fn compile_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        expression: &Expression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let value = self.compile_expression_uncoerced(environment, expression)?;
        if environment.did_return {
            self.release_moved_ownership(environment, expression.syntax().id)?;
            return Ok(value);
        }
        let value = if let Some(coercion) = self
            .typed_module
            .coercion_for(expression.syntax().id)
            .cloned()
        {
            let source = substitute_type(coercion.source, &self.active_type_substitutions);
            let target = substitute_type(coercion.target, &self.active_type_substitutions);
            self.coerce_value(value, &source, &target, expression.syntax().span.clone())?
        } else {
            value
        };
        self.release_moved_ownership(environment, expression.syntax().id)?;
        Ok(value)
    }

    fn compile_expression_uncoerced(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        expression: &Expression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if !matches!(expression, Expression::Index(_))
            && let Some(dispatch) = self
                .typed_module
                .trait_dispatch_for(expression.syntax().id)
                .cloned()
        {
            let trait_id = self
                .typed_module
                .resolved()
                .trait_for_method(dispatch.method)
                .expect("trait method owner");
            let arguments = dispatch
                .arguments
                .into_iter()
                .map(|argument| substitute_type(argument, &self.active_type_substitutions))
                .collect::<Vec<_>>();
            let arguments = self
                .typed_module
                .complete_trait_arguments(trait_id, &arguments)
                .ok_or_else(|| {
                    Diagnostic::new(
                        expression.syntax().span.clone(),
                        "could not infer functional dependency arguments",
                    )
                })?;
            if arguments.iter().any(contains_type_parameter) {
                return Err(Diagnostic::new(
                    expression.syntax().span.clone(),
                    "trait method arguments are not fully specialized",
                ));
            }
            let function = self.trait_method_code(
                trait_id,
                &arguments,
                dispatch.method,
                expression.syntax().span.clone(),
            )?;
            let closure = if let Some(function_id) =
                self.typed_module
                    .trait_impl_method(trait_id, &arguments, dispatch.method)
            {
                self.build_closure_with_code(
                    environment,
                    function_id,
                    function,
                    expression.syntax().span.clone(),
                )?
            } else {
                self.build_closure_value(
                    function,
                    self.context.ptr_type(AddressSpace::default()).const_null(),
                )?
            };
            return Ok(closure.as_any_value_enum());
        }
        match expression {
            Expression::Function(function) => {
                let id = self
                    .typed_module
                    .function_for(function.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            function.syntax.span.clone(),
                            "unresolved function expression",
                        )
                    })?;
                let template = self
                    .typed_module
                    .type_of_function(id)
                    .expect("checked function expression");
                if contains_type_parameter(&CheckedType::Function(template.clone())) {
                    let concrete = substitute_type(
                        CheckedType::Function(template.clone()),
                        &self.active_type_substitutions,
                    );
                    let CheckedType::Function(concrete) = concrete else {
                        unreachable!();
                    };
                    let code = self.ensure_function_specialization(id, &concrete)?;
                    return self
                        .build_closure_with_code(
                            environment,
                            id,
                            code,
                            function.syntax.span.clone(),
                        )
                        .map(|closure| closure.as_any_value_enum());
                }
                self.build_closure(environment, id, function.syntax.span.clone())
                    .map(|closure| closure.as_any_value_enum())
            }
            Expression::Satisfies(satisfies) => {
                self.compile_expression(environment, &satisfies.value)
            }
            Expression::Match(match_) => self.compile_match_expression(environment, match_),
            Expression::Logical(logical) => self.compile_logical_expression(environment, logical),
            Expression::Loop(loop_) => self.compile_loop_expression(environment, loop_),
            Expression::Resource(resource) => {
                let expected = self
                    .typed_module
                    .resource_for_expression(resource.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(resource.syntax.span.clone(), "unchecked resource access")
                    })?;
                environment
                    .resources
                    .iter()
                    .rev()
                    .find(|candidate| candidate.resource.value_type == expected.value_type)
                    .map(|bound| {
                        if bound.indirect {
                            let pointer = value_as_basic(bound.value)
                                .expect("borrowed resource pointer is first-class")
                                .into_pointer_value();
                            self.builder
                                .build_load(
                                    self.compile_type(&expected.value_type)
                                        .expect("resource type compiles"),
                                    pointer,
                                    "resource.borrow",
                                )
                                .expect("resource load")
                                .as_any_value_enum()
                        } else {
                            bound.value
                        }
                    })
                    .ok_or_else(|| {
                        Diagnostic::new(
                            resource.syntax.span.clone(),
                            format!("resource `{}` is not available", expected.value_type),
                        )
                    })
            }
            Expression::With(with) => {
                let resource = self
                    .typed_module
                    .resource_for_expression(with.syntax.id)
                    .cloned()
                    .ok_or_else(|| {
                        Diagnostic::new(with.syntax.span.clone(), "unchecked resource provider")
                    })?;
                let value = self.compile_expression(environment, &with.value)?;
                if environment.did_return {
                    return Ok(self.unit_value());
                }
                let copy = self
                    .typed_module
                    .is_copy_in_function(&resource.value_type, environment.function_id);
                let borrow_provider = with.mutable || !copy;
                let stored = if borrow_provider
                    && let Ok((pointer, _, _)) =
                        self.compile_place_pointer(environment, &with.value)
                {
                    pointer.as_any_value_enum()
                } else {
                    let ty = self.compile_type(&resource.value_type)?;
                    let pointer = self
                        .builder
                        .build_alloca(ty, "resource.provider")
                        .map_err(compiler_diagnostic)?;
                    self.builder
                        .build_store(
                            pointer,
                            value_as_basic(value).ok_or_else(|| {
                                Diagnostic::new(
                                    with.value.syntax().span.clone(),
                                    "resource provider is not first-class",
                                )
                            })?,
                        )
                        .map_err(compiler_diagnostic)?;
                    pointer.as_any_value_enum()
                };
                environment.resources.push(BoundResource {
                    resource,
                    value: stored,
                    indirect: true,
                });
                let reactive = self
                    .typed_module
                    .is_reactive_type(&environment.resources.last().unwrap().resource.value_type);
                if reactive {
                    let scope = value_as_basic(value)
                        .expect("Reactive is first-class")
                        .into_pointer_value();
                    environment.reactive_scopes.push(scope);
                }
                let result =
                    self.compile_expression(environment, &Expression::Block(with.body.clone()));
                if reactive {
                    if !environment.did_return {
                        self.dispose_reactive_scopes(
                            environment,
                            environment.reactive_scopes.len() - 1,
                            with.syntax.span.clone(),
                        )?;
                    }
                    environment.reactive_scopes.pop();
                }
                environment.resources.pop();
                result
            }
            Expression::Block(block) => {
                let owned_before = environment.owned_order.len();
                self.predeclare_checked_bindings(environment, &block.items)?;
                let mut value = None;
                for item in &block.items {
                    value = self.compile_item(environment, item)?;
                    if environment.did_return {
                        break;
                    }
                }
                let result = value.unwrap_or_else(|| self.unit_value());
                if !environment.did_return {
                    self.drop_owned_since(environment, owned_before, block.syntax.span.clone())?;
                }
                Ok(result)
            }
            Expression::Product(product) => self.compile_product_expression(environment, product),
            Expression::RepeatedProduct(repeated) => {
                self.compile_repeated_product_expression(environment, repeated)
            }
            Expression::Call(call) => {
                if self
                    .typed_module
                    .resolved()
                    .primitive_macro_for(call.syntax.id)
                    .is_some()
                {
                    return self.compile_c_string_macro(call);
                }
                if let Some(symbol) = self.typed_module.symbol_for(call.callee.syntax().id)
                    && let Some(type_id) = self.typed_module.resolved().constructor_type(symbol)
                {
                    let value = self.compile_expression(environment, &call.argument)?;
                    if environment.did_return {
                        return Ok(self.unit_value());
                    }
                    if self.typed_module.resolved().recursive_construction(type_id)
                        == Some(crate::RecursiveConstruction::ManagedReference)
                    {
                        let constructor_type = self
                            .typed_module
                            .coercion_for(expression.syntax().id)
                            .map(|coercion| coercion.source.clone())
                            .or_else(|| self.concrete_expression_type(expression));
                        let CheckedType::Ref(payload) = constructor_type.ok_or_else(|| {
                            Diagnostic::new(
                                call.syntax.span.clone(),
                                "Ref constructor has no concrete result type",
                            )
                        })?
                        else {
                            return Err(Diagnostic::new(
                                call.syntax.span.clone(),
                                "Ref constructor has an invalid result type",
                            ));
                        };
                        let value = value_as_basic(value).ok_or_else(|| {
                            Diagnostic::new(
                                call.argument.syntax().span.clone(),
                                "Ref payload is not first-class",
                            )
                        })?;
                        return self
                            .build_ref_value(value, &payload, call.syntax.span.clone())
                            .map(|value| value.as_any_value_enum());
                    }
                    return Ok(value);
                }
                self.compile_call_expression(environment, call)
            }
            Expression::Access(access) => {
                if let Some(symbol) = self.typed_module.symbol_for(access.syntax.id) {
                    if self
                        .typed_module
                        .resolved()
                        .singleton_type(symbol)
                        .is_some()
                    {
                        return Ok(self.unit_value());
                    }
                    return self.compile_symbol_value(
                        environment,
                        symbol,
                        self.typed_module
                            .resolved()
                            .requires_initialization_check(access.syntax.id)
                            || self.typed_module.has_mutable_storage(symbol),
                        access.syntax.span.clone(),
                        "value is not available here".to_owned(),
                    );
                }
                let value = self.compile_expression(environment, &access.value)?;
                if environment.did_return {
                    return Ok(self.unit_value());
                }
                let checked = self
                    .typed_module
                    .access_for(access.syntax.id)
                    .cloned()
                    .ok_or_else(|| {
                        Diagnostic::new(access.syntax.span.clone(), "missing checked access")
                    })?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        access.value.syntax().span.clone(),
                        "element access requires a product value",
                    )
                })?;
                if let crate::CheckedAccess::Representation { dereference } = &checked {
                    return if let Some(payload) = dereference {
                        self.load_ref_payload(value, payload, access.syntax.span.clone())
                            .map(|value| value.as_any_value_enum())
                    } else {
                        Ok(value.as_any_value_enum())
                    };
                }
                let crate::CheckedAccess::Product {
                    index,
                    dereference,
                    erased,
                    scalar,
                } = checked
                else {
                    unreachable!("representation access handled above")
                };
                if scalar {
                    return Ok(value.as_any_value_enum());
                }
                if erased {
                    let BasicValueEnum::StructValue(reference) = value else {
                        return Err(Diagnostic::new(
                            access.value.syntax().span.clone(),
                            "slice has an invalid representation",
                        ));
                    };
                    let pointer = self
                        .builder
                        .build_extract_value(reference, 0, "slice.pointer")
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value();
                    let length = self
                        .builder
                        .build_extract_value(reference, 1, "slice.length")
                        .map_err(compiler_diagnostic)?
                        .into_int_value();
                    let position = self.size_type.const_int(index as u64, false);
                    return self
                        .compile_index_load(
                            pointer,
                            position,
                            length,
                            self.typed_module
                                .type_of_expression(access.syntax.id)
                                .cloned()
                                .unwrap_or(CheckedType::Error),
                            access.syntax.span.clone(),
                        )
                        .map(|value| value.as_any_value_enum());
                }
                let value = if let Some(payload) = &dereference {
                    self.load_ref_payload(value, payload, access.syntax.span.clone())?
                } else {
                    value
                };
                let BasicValueEnum::StructValue(value) = value else {
                    return Err(Diagnostic::new(
                        access.value.syntax().span.clone(),
                        "element access requires a product value",
                    ));
                };
                self.builder
                    .build_extract_value(value, index as u32, "element")
                    .map(|value| value.as_any_value_enum())
                    .map_err(|error| Diagnostic::new(access.syntax.span.clone(), error.to_string()))
            }
            Expression::Index(index) => self.compile_index_expression(environment, index),
            Expression::Name(name) => {
                let symbol = self
                    .typed_module
                    .symbol_for(name.syntax.id)
                    .ok_or_else(|| Diagnostic::new(name.syntax.span.clone(), "unresolved name"))?;
                if self
                    .typed_module
                    .resolved()
                    .singleton_type(symbol)
                    .is_some()
                {
                    return Ok(self.unit_value());
                }
                if self
                    .typed_module
                    .resolved()
                    .constructor_type(symbol)
                    .is_some()
                {
                    let Some(CheckedType::Function(function_type)) =
                        self.concrete_expression_type(expression)
                    else {
                        return Err(Diagnostic::new(
                            name.syntax.span.clone(),
                            "constructor has no concrete function type",
                        ));
                    };
                    let code = self.ensure_constructor_adapter(symbol, &function_type)?;
                    let environment = self.context.ptr_type(AddressSpace::default()).const_null();
                    return self
                        .build_closure_value(code, environment)
                        .map(|closure| closure.as_any_value_enum());
                }
                if let Some(function_id) = self.function_symbols.get(&symbol).copied()
                    && let Some(CheckedType::Function(function_type)) =
                        self.concrete_expression_type(expression)
                    && contains_type_parameter(&CheckedType::Function(
                        self.typed_module
                            .type_of_function(function_id)
                            .expect("checked function")
                            .clone(),
                    ))
                {
                    if self
                        .typed_module
                        .resolved()
                        .requires_initialization_check(name.syntax.id)
                    {
                        self.check_symbol_initialization(
                            environment,
                            symbol,
                            name.syntax.span.clone(),
                        )?;
                    }
                    let code = self.ensure_function_specialization(function_id, &function_type)?;
                    return self
                        .build_closure_with_code(
                            environment,
                            function_id,
                            code,
                            name.syntax.span.clone(),
                        )
                        .map(|closure| closure.as_any_value_enum());
                }
                self.compile_symbol_value(
                    environment,
                    symbol,
                    self.typed_module
                        .resolved()
                        .requires_initialization_check(name.syntax.id)
                        || self.typed_module.has_mutable_storage(symbol),
                    name.syntax.span.clone(),
                    format!("value `{}` is not available here", name.name),
                )
            }
            Expression::String(string) => {
                let value = crate::string_literal::decode(&string.literal)
                    .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
                let source = self
                    .builder
                    .build_global_string_ptr(&value, "string")
                    .map_err(|error| {
                        Diagnostic::new(string.syntax.span.clone(), error.to_string())
                    })?
                    .as_pointer_value();
                let length = self.size_type.const_int(value.len() as u64, false);
                let pointer =
                    self.build_gc_allocation(length, "string.data", string.syntax.span.clone())?;
                self.builder
                    .build_memcpy(pointer, 1, source, 1, length)
                    .map_err(|error| {
                        Diagnostic::new(string.syntax.span.clone(), error.to_string())
                    })?;
                self.build_string_value(pointer, length, string.syntax.span.clone())
                    .map(|value| value.as_any_value_enum())
            }
            Expression::StringTemplate(template) => {
                self.compile_string_template(environment, template)
            }
            Expression::CString(string) => self.compile_c_string_literal(string),
            Expression::SyntaxArgument(argument) => Err(Diagnostic::new(
                argument.syntax.span.clone(),
                "unexpanded grouped syntax argument",
            )),
            Expression::Quote(quote) => Err(Diagnostic::new(
                quote.syntax.span.clone(),
                format!("unexpanded `{}` expression", quote.kind.name()),
            )),
            Expression::Splice(splice) => Err(Diagnostic::new(
                splice.syntax.span.clone(),
                "unexpanded splice expression",
            )),
            Expression::Integer(integer) => {
                let value = integer.literal.parse::<u64>().map_err(|_| {
                    Diagnostic::new(integer.syntax.span.clone(), "integer literal is too large")
                })?;
                let integer_type = self
                    .typed_module
                    .type_of_expression(integer.syntax.id)
                    .and_then(CheckedType::integer_type)
                    .unwrap_or(IntegerType::I32);
                let llvm_type = self.compile_integer_type(integer_type);
                let width = llvm_type.get_bit_width();
                let value_bits = if integer_type.is_signed() {
                    width - 1
                } else {
                    width
                };
                if value_bits < 64 && value > ((1_u64 << value_bits) - 1) {
                    return Err(Diagnostic::new(
                        integer.syntax.span.clone(),
                        format!(
                            "integer literal `{}` does not fit in `{}`",
                            integer.literal,
                            integer_type.name()
                        ),
                    ));
                }
                Ok(llvm_type.const_int(value, false).as_any_value_enum())
            }
            Expression::Float(float) => {
                let float_type = self
                    .typed_module
                    .type_of_expression(float.syntax.id)
                    .and_then(CheckedType::float_type)
                    .unwrap_or(FloatType::F64);
                let value = match float_type {
                    FloatType::F32 => float.literal.parse::<f32>().map(f64::from),
                    FloatType::F64 => float.literal.parse::<f64>(),
                }
                .map_err(|_| Diagnostic::new(float.syntax.span.clone(), "invalid float literal"))?;
                if !value.is_finite() {
                    return Err(Diagnostic::new(
                        float.syntax.span.clone(),
                        format!(
                            "float literal `{}` does not fit in `{}`",
                            float.literal,
                            float_type.name()
                        ),
                    ));
                }
                let llvm_type = self.compile_float_type(float_type);
                Ok(llvm_type.const_float(value).as_any_value_enum())
            }
            Expression::VisibilityArgument(_) => {
                unreachable!("visibility syntax must be eliminated during macro expansion")
            }
            Expression::Binary(_) => unreachable!("binary expression reached code generation"),
        }
    }

    fn trait_method_code(
        &mut self,
        trait_id: crate::TraitId,
        arguments: &[CheckedType],
        method: crate::TraitMethodId,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let function_type = self
            .typed_module
            .instantiated_trait_method_type(trait_id, arguments, method)
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "trait method has no concrete function type")
            })?;
        if let Some(function_id) = self
            .typed_module
            .trait_impl_method(trait_id, arguments, method)
        {
            if let Some(function) = self.functions.get(&function_id).copied() {
                return Ok(function);
            }
            return self.ensure_function_specialization(function_id, &function_type);
        }
        let structural = self
            .typed_module
            .structural_trait_method(trait_id, arguments)
            .ok_or_else(|| Diagnostic::new(span.clone(), "no trait implementation is available"))?;
        self.structural_trait_method_code(structural, arguments, &function_type, span)
    }

    fn structural_trait_method_code(
        &mut self,
        structural: crate::StructuralTraitMethod,
        arguments: &[CheckedType],
        function_type: &CheckedFunctionType,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let key = (structural, format!("{arguments:?}"));
        if let Some(function) = self.structural_trait_codes.get(&key).copied() {
            return Ok(function);
        }
        let llvm_type = self.compile_closure_function_type(function_type)?;
        let name = format!(
            "__staple_structural_{:?}_{}",
            structural,
            self.structural_trait_codes.len()
        );
        let function = self.llvm_module.add_function(&name, llvm_type, None);
        self.structural_trait_codes.insert(key, function);
        let previous = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);
        let parameters = function.get_params();
        let values = &parameters[1..];
        let result = match structural {
            crate::StructuralTraitMethod::Debug => {
                self.compile_structural_debug_body(values, &arguments[0], span.clone())?
            }
            crate::StructuralTraitMethod::Index => self.compile_structural_index_body(
                values,
                &arguments[0],
                &arguments[2],
                span.clone(),
            )?,
            crate::StructuralTraitMethod::MutateIndex => self.compile_structural_mutate_body(
                values,
                &arguments[0],
                &arguments[2],
                span.clone(),
            )?,
            crate::StructuralTraitMethod::IntoIterator => {
                self.compile_structural_into_iterator_body(values, span.clone())?
            }
            crate::StructuralTraitMethod::Iterator => self.compile_structural_next_body(
                values,
                &arguments[0],
                &arguments[1],
                &function_type.result,
                span.clone(),
            )?,
        };
        self.builder
            .build_return(Some(&result))
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        if let Some(block) = previous {
            self.builder.position_at_end(block);
        }
        Ok(function)
    }

    fn compile_structural_debug_body(
        &mut self,
        values: &[BasicValueEnum<'context>],
        target: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let [value, formatter] = values else {
            return Err(Diagnostic::new(span, "invalid structural Debug arguments"));
        };
        let BasicValueEnum::StructValue(value) = value else {
            return Err(Diagnostic::new(span, "invalid structural Debug value"));
        };
        if let CheckedType::Sum(sum) = target {
            return self.compile_structural_sum_debug_body(*value, *formatter, sum, span);
        }
        let CheckedType::Product(product) = target else {
            return Err(Diagnostic::new(
                span,
                "structural Debug requires a product or sum",
            ));
        };
        self.compile_formatter_write_literal(*formatter, "(", span.clone())?;
        let debug_trait = self
            .typed_module
            .resolved()
            .standard_trait("Debug")
            .ok_or_else(|| Diagnostic::new(span.clone(), "standard Debug trait is unavailable"))?;
        let debug_method = self
            .typed_module
            .resolved()
            .traits()
            .get(&debug_trait)
            .and_then(|trait_| trait_.methods.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "standard Debug method is unavailable"))?;
        for (index, element) in product.elements.iter().enumerate() {
            if index != 0 {
                self.compile_formatter_write_literal(*formatter, ", ", span.clone())?;
            }
            if let Some(name) = &element.name {
                self.compile_formatter_write_literal(*formatter, name, span.clone())?;
                self.compile_formatter_write_literal(*formatter, ": ", span.clone())?;
            }
            let field = self
                .builder
                .build_extract_value(*value, index as u32, "debug.element")
                .map_err(compiler_diagnostic)?;
            let arguments = vec![element.value_type.clone()];
            let function =
                self.trait_method_code(debug_trait, &arguments, debug_method, span.clone())?;
            self.builder
                .build_direct_call(
                    function,
                    &[
                        self.context
                            .ptr_type(AddressSpace::default())
                            .const_null()
                            .into(),
                        field.into(),
                        (*formatter).into(),
                    ],
                    "debug.fmt",
                )
                .map_err(compiler_diagnostic)?;
        }
        self.compile_formatter_write_literal(*formatter, ")", span.clone())?;
        value_as_basic(self.unit_value()).ok_or_else(|| Diagnostic::new(span, "invalid unit value"))
    }

    fn compile_structural_sum_debug_body(
        &mut self,
        value: inkwell::values::StructValue<'context>,
        formatter: BasicValueEnum<'context>,
        sum: &crate::CheckedSumType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let tag = self
            .builder
            .build_extract_value(value, 0, "debug.sum.tag")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "sum Debug is not in a function"))?;
        let merge = self.context.append_basic_block(function, "debug.sum.done");
        let cases = sum
            .alternatives
            .iter()
            .enumerate()
            .map(|(index, _)| {
                (
                    self.context.i32_type().const_int(index as u64, false),
                    self.context.append_basic_block(function, "debug.sum.case"),
                )
            })
            .collect::<Vec<_>>();
        self.builder
            .build_switch(tag, merge, &cases)
            .map_err(compiler_diagnostic)?;
        let debug_trait = self
            .typed_module
            .resolved()
            .standard_trait("Debug")
            .ok_or_else(|| Diagnostic::new(span.clone(), "standard Debug trait is unavailable"))?;
        let debug_method = self
            .typed_module
            .resolved()
            .traits()
            .get(&debug_trait)
            .and_then(|trait_| trait_.methods.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "standard Debug method is unavailable"))?;
        for (index, alternative) in sum.alternatives.iter().enumerate() {
            self.builder.position_at_end(cases[index].1);
            let payload = self.extract_sum_alternative(value, sum, index, span.clone())?;
            let function = self.trait_method_code(
                debug_trait,
                std::slice::from_ref(alternative),
                debug_method,
                span.clone(),
            )?;
            self.builder
                .build_direct_call(
                    function,
                    &[
                        self.context
                            .ptr_type(AddressSpace::default())
                            .const_null()
                            .into(),
                        payload.into(),
                        formatter.into(),
                    ],
                    "debug.sum.fmt",
                )
                .map_err(compiler_diagnostic)?;
            self.builder
                .build_unconditional_branch(merge)
                .map_err(compiler_diagnostic)?;
        }
        self.builder.position_at_end(merge);
        value_as_basic(self.unit_value()).ok_or_else(|| Diagnostic::new(span, "invalid unit value"))
    }

    fn compile_formatter_write_literal(
        &mut self,
        formatter: BasicValueEnum<'context>,
        literal: &str,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let function_id = self
            .typed_module
            .resolved()
            .functions()
            .iter()
            .find(|function| standard_function_name_matches(&function.name, "formatter_write"))
            .map(|function| function.id)
            .ok_or_else(|| {
                Diagnostic::new(
                    span.clone(),
                    "Formatter.write implementation is unavailable",
                )
            })?;
        let function_type = self
            .typed_module
            .type_of_function(function_id)
            .cloned()
            .ok_or_else(|| Diagnostic::new(span.clone(), "Formatter.write has no checked type"))?;
        let function = self.ensure_function_specialization(function_id, &function_type)?;
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

    fn compile_string_template(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        template: &crate::StringTemplateExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let new_id = self.standard_function_named("formatter_new", template.syntax.span.clone())?;
        let new_type = self
            .typed_module
            .type_of_function(new_id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(
                    template.syntax.span.clone(),
                    "Formatter.new has no checked type",
                )
            })?;
        let new = self.ensure_function_specialization(new_id, &new_type)?;
        let formatter = self
            .builder
            .build_direct_call(
                new,
                &[self
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null()
                    .into()],
                "template.formatter",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic();
        let storage = self
            .builder
            .build_alloca(formatter.get_type(), "template.formatter.storage")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(storage, formatter)
            .map_err(compiler_diagnostic)?;
        for part in &template.parts {
            match part {
                crate::StringTemplatePart::Literal(literal) => {
                    self.compile_formatter_write_literal(
                        storage.into(),
                        literal,
                        template.syntax.span.clone(),
                    )?;
                }
                crate::StringTemplatePart::Interpolation(interpolation) => {
                    let value = self.compile_expression(environment, &interpolation.expression)?;
                    if environment.did_return {
                        return Ok(self.unit_value());
                    }
                    let value_type = self
                        .concrete_expression_type(&interpolation.expression)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                interpolation.expression.syntax().span.clone(),
                                "interpolation has no concrete type",
                            )
                        })?;
                    let trait_name = match interpolation.format {
                        crate::StringInterpolationFormat::Display => "Display",
                        crate::StringInterpolationFormat::Debug => "Debug",
                    };
                    let trait_id = self
                        .typed_module
                        .resolved()
                        .standard_trait(trait_name)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                template.syntax.span.clone(),
                                "formatting trait is unavailable",
                            )
                        })?;
                    let method = self.typed_module.resolved().traits()[&trait_id].methods[0];
                    let function = self.trait_method_code(
                        trait_id,
                        std::slice::from_ref(&value_type),
                        method,
                        interpolation.expression.syntax().span.clone(),
                    )?;
                    let value = value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(
                            interpolation.expression.syntax().span.clone(),
                            "interpolation value has no runtime representation",
                        )
                    })?;
                    self.builder
                        .build_direct_call(
                            function,
                            &[
                                self.context
                                    .ptr_type(AddressSpace::default())
                                    .const_null()
                                    .into(),
                                value.into(),
                                storage.into(),
                            ],
                            "template.fmt",
                        )
                        .map_err(compiler_diagnostic)?;
                }
            }
        }
        let finish_id =
            self.standard_function_named("formatter_finish", template.syntax.span.clone())?;
        let finish_type = self
            .typed_module
            .type_of_function(finish_id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(
                    template.syntax.span.clone(),
                    "Formatter.finish has no checked type",
                )
            })?;
        let finish = self.ensure_function_specialization(finish_id, &finish_type)?;
        let formatter = self
            .builder
            .build_load(formatter.get_type(), storage, "template.finished.formatter")
            .map_err(compiler_diagnostic)?;
        Ok(self
            .builder
            .build_direct_call(
                finish,
                &[
                    self.context
                        .ptr_type(AddressSpace::default())
                        .const_null()
                        .into(),
                    formatter.into(),
                ],
                "template.finish",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .as_any_value_enum())
    }

    fn standard_function_named(
        &self,
        name: &str,
        span: Span,
    ) -> CodeGenerationResult<crate::FunctionId> {
        self.typed_module
            .resolved()
            .functions()
            .iter()
            .find(|function| standard_function_name_matches(&function.name, name))
            .map(|function| function.id)
            .ok_or_else(|| {
                Diagnostic::new(span, format!("standard function `{name}` is unavailable"))
            })
    }

    fn compile_structural_index_body(
        &mut self,
        values: &[BasicValueEnum<'context>],
        target: &CheckedType,
        output: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let [value, BasicValueEnum::IntValue(position)] = values else {
            return Err(Diagnostic::new(span, "invalid structural Index arguments"));
        };
        if let CheckedType::Product(product) = target
            && product.homogeneous_element().is_none()
        {
            let BasicValueEnum::StructValue(product_value) = value else {
                return Err(Diagnostic::new(
                    span,
                    "heterogeneous product has an invalid representation",
                ));
            };
            let length = self
                .size_type
                .const_int(product.elements.len() as u64, false);
            let out = self
                .builder
                .build_int_compare(
                    inkwell::IntPredicate::UGE,
                    *position,
                    length,
                    "index.out_of_bounds",
                )
                .map_err(compiler_diagnostic)?;
            self.build_trap_if(out, span.clone())?;
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
            let cases = product
                .elements
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    (
                        self.size_type.const_int(index as u64, false),
                        self.context.append_basic_block(function, "index.case"),
                    )
                })
                .collect::<Vec<_>>();
            self.builder
                .build_switch(*position, merge, &cases)
                .map_err(compiler_diagnostic)?;
            for (index, element) in product.elements.iter().enumerate() {
                self.builder.position_at_end(cases[index].1);
                let field = self
                    .builder
                    .build_extract_value(*product_value, index as u32, "index.field")
                    .map_err(compiler_diagnostic)?;
                let field = self.coerce_value(
                    field.as_any_value_enum(),
                    &element.value_type,
                    output,
                    span.clone(),
                )?;
                let field = value_as_basic(field).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "indexed field is not first-class")
                })?;
                self.builder
                    .build_store(output_slot, field)
                    .map_err(compiler_diagnostic)?;
                self.builder
                    .build_unconditional_branch(merge)
                    .map_err(compiler_diagnostic)?;
            }
            self.builder.position_at_end(merge);
            return self
                .builder
                .build_load(output_type, output_slot, "index.value")
                .map_err(|error| Diagnostic::new(span, error.to_string()));
        }

        let (pointer, length) = self.structural_index_storage(*value, target, span.clone())?;
        self.compile_index_load(pointer, *position, length, output.clone(), span)
    }

    fn structural_index_storage(
        &mut self,
        value: BasicValueEnum<'context>,
        target: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<(
        inkwell::values::PointerValue<'context>,
        inkwell::values::IntValue<'context>,
    )> {
        match target {
            CheckedType::Product(product) => {
                let llvm_type = self.compile_type(target)?;
                let pointer = self
                    .builder
                    .build_alloca(llvm_type, "index.product")
                    .map_err(compiler_diagnostic)?;
                self.builder
                    .build_store(pointer, value)
                    .map_err(compiler_diagnostic)?;
                Ok((
                    pointer,
                    self.size_type
                        .const_int(product.elements.len() as u64, false),
                ))
            }
            CheckedType::Ref(payload) => match payload.as_ref() {
                CheckedType::Product(product) => {
                    let BasicValueEnum::PointerValue(pointer) = value else {
                        return Err(Diagnostic::new(
                            span,
                            "reference has an invalid representation",
                        ));
                    };
                    Ok((
                        pointer,
                        self.size_type
                            .const_int(product.elements.len() as u64, false),
                    ))
                }
                _ => Err(Diagnostic::new(span, "invalid structural Index target")),
            },
            CheckedType::Slice(_) => {
                let BasicValueEnum::StructValue(slice) = value else {
                    return Err(Diagnostic::new(span, "slice has an invalid representation"));
                };
                let pointer = self
                    .builder
                    .build_extract_value(slice, 0, "index.pointer")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let length = self
                    .builder
                    .build_extract_value(slice, 1, "index.length")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                Ok((pointer, length))
            }
            _ => Err(Diagnostic::new(span, "invalid structural Index target")),
        }
    }

    fn compile_structural_mutate_body(
        &mut self,
        values: &[BasicValueEnum<'context>],
        target: &CheckedType,
        element: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let [
            BasicValueEnum::PointerValue(reference_pointer),
            BasicValueEnum::IntValue(position),
            replacement,
        ] = values
        else {
            return Err(Diagnostic::new(
                span,
                "invalid structural MutateIndex arguments",
            ));
        };
        // The mutable `Target` parameter passes by address either way, but a
        // by-value product's address *is* its storage, while a `Ref` target
        // is itself the value at that address and must be loaded and
        // unwrapped to reach the storage it points to.
        let (pointer, length) = match target {
            CheckedType::Product(product) => (
                *reference_pointer,
                self.size_type
                    .const_int(product.elements.len() as u64, false),
            ),
            CheckedType::Ref(_) | CheckedType::Slice(_) => {
                let reference = self
                    .builder
                    .build_load(
                        self.compile_type(target)?,
                        *reference_pointer,
                        "mutation.target",
                    )
                    .map_err(compiler_diagnostic)?;
                self.structural_index_storage(reference, target, span.clone())?
            }
            _ => {
                return Err(Diagnostic::new(
                    span,
                    "invalid structural MutateIndex target",
                ));
            }
        };
        self.compile_structural_replace(
            pointer,
            *position,
            length,
            *replacement,
            element,
            span.clone(),
        )?;
        value_as_basic(self.unit_value())
            .ok_or_else(|| Diagnostic::new(span, "unit is not first-class"))
    }

    fn compile_structural_into_iterator_body(
        &mut self,
        values: &[BasicValueEnum<'context>],
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        // `values` holds the source product's own elements flattened as
        // separate parameters (the same top-level-product flattening
        // `compile_structural_index_body` observes for its `target`), so
        // the source value itself must first be rebuilt as a single
        // struct before being paired with the initial cursor.
        let source_value = self.build_product_value(values, span.clone())?;
        let cursor = self.size_type.const_int(0, false);
        self.build_product_value(&[source_value, cursor.into()], span)
    }

    /// `next` for a structurally-iterable product: the `Iter` is `(P,
    /// USize)` (the product plus a cursor). If the cursor is in range, the
    /// field it names is extracted with the same per-index LLVM `switch`
    /// used by `compile_structural_index_body` and coerced into `Item`;
    /// otherwise the `Iter` is returned unchanged via `Done`. Which sum
    /// alternative is `Done`/`Yield` is found by matching `result`'s
    /// alternatives against their expected (already-substituted)
    /// representation, since `IterStep.Done`/`IterStep.Yield` are ordinary
    /// prelude `Distinct` types, not compiler intrinsics.
    fn compile_structural_next_body(
        &mut self,
        values: &[BasicValueEnum<'context>],
        iter: &CheckedType,
        item: &CheckedType,
        result: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        // `Iter` is itself a 2-element top-level product `(P, USize)`, so
        // it arrives flattened into two separate parameters, the same way
        // `compile_structural_index_body` receives `target`/`position`
        // separately rather than as one struct.
        let [product_value, BasicValueEnum::IntValue(cursor)] = values else {
            return Err(Diagnostic::new(
                span,
                "invalid structural Iterator arguments",
            ));
        };
        let CheckedType::Product(iter_product) = iter else {
            return Err(Diagnostic::new(span, "invalid structural Iterator target"));
        };
        let CheckedType::Product(product) = &iter_product.elements[0].value_type else {
            return Err(Diagnostic::new(span, "invalid structural Iterator target"));
        };
        let CheckedType::Sum(result_sum) = result else {
            return Err(Diagnostic::new(span, "invalid structural Iterator result"));
        };
        let yield_representation = CheckedType::Product(CheckedProductType {
            elements: vec![
                CheckedTypeElement {
                    name: None,
                    value_type: item.clone(),
                    default: None,
                },
                CheckedTypeElement {
                    name: None,
                    value_type: iter.clone(),
                    default: None,
                },
            ],
            variadic: false,
        });
        let done_alternative = result_sum
            .alternatives
            .iter()
            .find(|alternative| {
                matches!(alternative, CheckedType::Distinct { representation, .. } if representation.as_ref() == iter)
            })
            .cloned()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing `IterStep.Done` alternative"))?;
        let yield_alternative = result_sum
            .alternatives
            .iter()
            .find(|alternative| {
                matches!(alternative, CheckedType::Distinct { representation, .. } if representation.as_ref() == &yield_representation)
            })
            .cloned()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing `IterStep.Yield` alternative"))?;

        let product_value = *product_value;
        let cursor = *cursor;

        let length = self
            .size_type
            .const_int(product.elements.len() as u64, false);
        let in_range = self
            .builder
            .build_int_compare(inkwell::IntPredicate::ULT, cursor, length, "next.in_range")
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

        self.builder.position_at_end(done_block);
        let iter_value = self.build_product_value(&[product_value, cursor.into()], span.clone())?;
        let done_value = self.coerce_value(
            iter_value.as_any_value_enum(),
            &done_alternative,
            result,
            span.clone(),
        )?;
        let done_value = value_as_basic(done_value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "iterator step is not first-class"))?;
        self.builder
            .build_store(result_slot, done_value)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(merge)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(dispatch_block);
        let cases = product
            .elements
            .iter()
            .enumerate()
            .map(|(index, _)| {
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

        for (index, element) in product.elements.iter().enumerate() {
            self.builder.position_at_end(cases[index].1);
            let field = self
                .builder
                .build_extract_value(
                    product_value.into_struct_value(),
                    index as u32,
                    "next.field",
                )
                .map_err(compiler_diagnostic)?;
            let item_value = self.coerce_value(
                field.as_any_value_enum(),
                &element.value_type,
                item,
                span.clone(),
            )?;
            let item_value = value_as_basic(item_value).ok_or_else(|| {
                Diagnostic::new(span.clone(), "iterated field is not first-class")
            })?;

            let next_index = self.size_type.const_int((index + 1) as u64, false);
            let next_iter_value =
                self.build_product_value(&[product_value, next_index.into()], span.clone())?;
            let yield_representation_value =
                self.build_product_value(&[item_value, next_iter_value], span.clone())?;
            let yielded = self.coerce_value(
                yield_representation_value.as_any_value_enum(),
                &yield_alternative,
                result,
                span.clone(),
            )?;
            let yielded = value_as_basic(yielded)
                .ok_or_else(|| Diagnostic::new(span.clone(), "iterator step is not first-class"))?;
            self.builder
                .build_store(result_slot, yielded)
                .map_err(compiler_diagnostic)?;
            self.builder
                .build_unconditional_branch(merge)
                .map_err(compiler_diagnostic)?;
        }

        self.builder.position_at_end(merge);
        self.builder
            .build_load(result_type, result_slot, "next.value")
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    fn compile_structural_replace(
        &mut self,
        pointer: inkwell::values::PointerValue<'context>,
        position: inkwell::values::IntValue<'context>,
        length: inkwell::values::IntValue<'context>,
        replacement: BasicValueEnum<'context>,
        element: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let out = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGE,
                position,
                length,
                "index.out_of_bounds",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(out, span.clone())?;
        let llvm_type = self.compile_type(element)?;
        let slot = unsafe {
            self.builder
                .build_gep(llvm_type, pointer, &[position], "index.element")
        }
        .map_err(compiler_diagnostic)?;
        if self.typed_module.type_needs_drop(element) {
            let old = self
                .builder
                .build_load(llvm_type, slot, "index.old")
                .map_err(compiler_diagnostic)?;
            self.compile_drop_value(old, element, span.clone())?;
        }
        self.builder
            .build_store(slot, replacement)
            .map(|_| ())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    fn unit_value(&self) -> AnyValueEnum<'context> {
        self.context
            .struct_type(&[], true)
            .const_zero()
            .as_any_value_enum()
    }

    fn compile_loop_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        loop_: &crate::LoopExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| {
                Diagnostic::new(loop_.syntax.span.clone(), "loop is not in a function")
            })?;
        let header = self.context.append_basic_block(function, "loop.body");
        let exit = self.context.append_basic_block(function, "loop.exit");
        self.builder
            .build_unconditional_branch(header)
            .map_err(|error| Diagnostic::new(loop_.syntax.span.clone(), error.to_string()))?;
        self.builder.position_at_end(header);

        environment.loops.push(LoopCodegenContext {
            header,
            exit,
            owned_before: environment.owned_order.len(),
            reactive_before: environment.reactive_scopes.len(),
            incoming: Vec::new(),
        });
        environment.did_return = false;
        self.compile_expression(environment, &Expression::Block(loop_.body.clone()))?;
        if !environment.did_return {
            self.builder
                .build_unconditional_branch(header)
                .map_err(|error| Diagnostic::new(loop_.syntax.span.clone(), error.to_string()))?;
        }
        let context = environment
            .loops
            .pop()
            .expect("loop code generation context");
        self.builder.position_at_end(exit);
        if context.incoming.is_empty() {
            self.builder
                .build_unreachable()
                .map_err(|error| Diagnostic::new(loop_.syntax.span.clone(), error.to_string()))?;
            environment.did_return = true;
            return Ok(self.unit_value());
        }

        environment.did_return = false;
        let result = self
            .typed_module
            .type_of_expression(loop_.syntax.id)
            .cloned()
            .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))
            .ok_or_else(|| {
                Diagnostic::new(
                    loop_.syntax.span.clone(),
                    "loop has no concrete result type",
                )
            })?;
        let result_type = self.compile_type(&result)?;
        let phi = self
            .builder
            .build_phi(result_type, "loop.value")
            .map_err(|error| Diagnostic::new(loop_.syntax.span.clone(), error.to_string()))?;
        let incoming = context
            .incoming
            .iter()
            .map(|(value, block)| (value as &dyn BasicValue<'context>, *block))
            .collect::<Vec<_>>();
        phi.add_incoming(&incoming);
        Ok(phi.as_basic_value().as_any_value_enum())
    }

    /// `&&`/`||` are not trait-based: codegen extracts `left`'s sum tag
    /// directly and branches on it, rather than going through
    /// `compile_match_pattern_branch`'s general (and here unneeded) pattern
    /// dispatch. Unlike a `match`'s sequential per-arm retry loop, the two
    /// outcomes here are genuinely exclusive successors of one conditional
    /// branch, so there is no need for `compile_match_expression`'s
    /// `branch_base`/`restore_local_state` bookkeeping between them — only
    /// the evaluated `right` branch does any work, and its temporaries are
    /// cleaned up with the same `drop_owned_since` used for a match arm.
    fn compile_logical_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        logical: &crate::LogicalExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let left = self.compile_expression(environment, &logical.left)?;
        if environment.did_return {
            return Ok(left);
        }
        let Some(left_value) = value_as_basic(left) else {
            return Err(Diagnostic::new(
                logical.left.syntax().span.clone(),
                "logical operand is not first-class",
            ));
        };
        let BasicValueEnum::StructValue(left_struct) = left_value else {
            return Err(Diagnostic::new(
                logical.left.syntax().span.clone(),
                "`Bool` value has an invalid representation",
            ));
        };
        let checked = self
            .typed_module
            .logical_for(logical.syntax.id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(logical.syntax.span.clone(), "missing checked logical")
            })?;
        let bool_type = substitute_type(checked.bool_type, &self.active_type_substitutions);
        let CheckedType::Sum(sum) = &bool_type else {
            return Err(Diagnostic::new(
                logical.syntax.span.clone(),
                "`&&`/`||` require `Bool` to be a sum type",
            ));
        };
        let true_index = sum
            .alternatives
            .iter()
            .position(|alternative| {
                matches!(alternative, CheckedType::Distinct { name, .. } if name == "True")
            })
            .ok_or_else(|| {
                Diagnostic::new(logical.syntax.span.clone(), "`Bool` has no `True` alternative")
            })?;
        let tag = self
            .builder
            .build_extract_value(left_struct, 0, "logical.tag")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let is_true = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                tag,
                self.context.i32_type().const_int(true_index as u64, false),
                "logical.is_true",
            )
            .map_err(compiler_diagnostic)?;

        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| {
                Diagnostic::new(
                    logical.syntax.span.clone(),
                    "`&&`/`||` is not in a function",
                )
            })?;
        let merge_block = self.context.append_basic_block(function, "logical.merge");
        let right_block = self.context.append_basic_block(function, "logical.right");
        let short_circuit_block = self
            .context
            .append_basic_block(function, "logical.short_circuit");
        let (true_target, false_target) = match logical.operator {
            crate::LogicalOperator::And => (right_block, short_circuit_block),
            crate::LogicalOperator::Or => (short_circuit_block, right_block),
        };
        self.builder
            .build_conditional_branch(is_true, true_target, false_target)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(short_circuit_block);
        self.builder
            .build_unconditional_branch(merge_block)
            .map_err(compiler_diagnostic)?;
        let mut incoming = vec![(left_value, short_circuit_block)];

        self.builder.position_at_end(right_block);
        let owned_before = environment.owned_order.len();
        environment.did_return = false;
        let right = self.compile_expression(environment, &logical.right)?;
        if !environment.did_return {
            let right_value = value_as_basic(right).ok_or_else(|| {
                Diagnostic::new(
                    logical.right.syntax().span.clone(),
                    "logical operand is not first-class",
                )
            })?;
            self.drop_owned_since(environment, owned_before, logical.syntax.span.clone())?;
            self.builder
                .build_unconditional_branch(merge_block)
                .map_err(compiler_diagnostic)?;
            let predecessor = self
                .builder
                .get_insert_block()
                .expect("logical right block");
            incoming.push((right_value, predecessor));
        } else {
            let cleanup_start = owned_before.min(environment.owned_order.len());
            for symbol in &environment.owned_order[cleanup_start..] {
                environment.owned.remove(symbol);
            }
            environment.owned_order.truncate(cleanup_start);
        }

        self.builder.position_at_end(merge_block);
        environment.did_return = false;
        let result_type = self.compile_type(&bool_type)?;
        let phi = self
            .builder
            .build_phi(result_type, "logical.value")
            .map_err(compiler_diagnostic)?;
        let incoming_refs = incoming
            .iter()
            .map(|(value, block)| (value as &dyn BasicValue<'context>, *block))
            .collect::<Vec<_>>();
        phi.add_incoming(&incoming_refs);
        Ok(phi.as_basic_value().as_any_value_enum())
    }

    fn compile_match_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        match_: &crate::MatchExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let subject = self.compile_expression(environment, &match_.subject)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let checked = self
            .typed_module
            .match_for(match_.syntax.id)
            .cloned()
            .ok_or_else(|| Diagnostic::new(match_.syntax.span.clone(), "missing checked match"))?;
        let source = substitute_type(checked.source, &self.active_type_substitutions);
        let Some(subject) = value_as_basic(subject) else {
            return Err(Diagnostic::new(
                match_.subject.syntax().span.clone(),
                "match subject is not first-class",
            ));
        };
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| {
                Diagnostic::new(match_.syntax.span.clone(), "match is not in a function")
            })?;
        let merge_block = self.context.append_basic_block(function, "match.merge");
        let mut incoming = Vec::new();
        let branch_base = environment.clone();
        let mut continuing_state = None;
        let mut terminating_state = None;
        for arm in &match_.arms {
            environment.restore_local_state(&branch_base);
            let owned_before = environment.owned_order.len();
            let arm_block = self.context.append_basic_block(function, "match.arm");
            let failure_block = self.context.append_basic_block(function, "match.next");
            self.compile_match_pattern_branch(
                environment,
                &arm.pattern,
                subject,
                &source,
                arm_block,
                failure_block,
            )?;
            self.builder.position_at_end(arm_block);
            environment.did_return = false;
            let value = self.compile_expression(environment, &arm.body)?;
            if !environment.did_return {
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        arm.body.syntax().span.clone(),
                        "match arm result is not first-class",
                    )
                })?;
                self.drop_owned_since(environment, owned_before, arm.syntax.span.clone())?;
                self.builder
                    .build_unconditional_branch(merge_block)
                    .map_err(|error| Diagnostic::new(arm.syntax.span.clone(), error.to_string()))?;
                let predecessor = self.builder.get_insert_block().expect("match arm block");
                incoming.push((value, predecessor));
                continuing_state = Some(environment.clone());
            } else {
                let cleanup_start = owned_before.min(environment.owned_order.len());
                for symbol in &environment.owned_order[cleanup_start..] {
                    environment.owned.remove(symbol);
                }
                environment.owned_order.truncate(cleanup_start);
                terminating_state = Some(environment.clone());
            }
            self.builder.position_at_end(failure_block);
        }

        self.builder
            .build_unreachable()
            .map_err(|error| Diagnostic::new(match_.syntax.span.clone(), error.to_string()))?;
        self.builder.position_at_end(merge_block);
        if incoming.is_empty() {
            self.builder
                .build_unreachable()
                .map_err(|error| Diagnostic::new(match_.syntax.span.clone(), error.to_string()))?;
            environment.did_return = true;
            if let Some(state) = terminating_state {
                environment.restore_local_state(&state);
            }
            return Ok(self.unit_value());
        }
        if let Some(state) = continuing_state {
            environment.restore_local_state(&state);
        }
        environment.did_return = false;
        let result = self
            .typed_module
            .type_of_expression(match_.syntax.id)
            .cloned()
            .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))
            .ok_or_else(|| {
                Diagnostic::new(
                    match_.syntax.span.clone(),
                    "match has no concrete result type",
                )
            })?;
        let result_type = self.compile_type(&result)?;
        let phi = self
            .builder
            .build_phi(result_type, "match.value")
            .map_err(|error| Diagnostic::new(match_.syntax.span.clone(), error.to_string()))?;
        let incoming = incoming
            .iter()
            .map(|(value, block)| (value as &dyn BasicValue<'context>, *block))
            .collect::<Vec<_>>();
        phi.add_incoming(&incoming);
        Ok(phi.as_basic_value().as_any_value_enum())
    }

    fn compile_match_pattern_branch(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        pattern: &Pattern,
        value: BasicValueEnum<'context>,
        value_type: &CheckedType,
        success: inkwell::basic_block::BasicBlock<'context>,
        failure: inkwell::basic_block::BasicBlock<'context>,
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::At(at) => {
                self.bind_pattern_value(
                    environment,
                    &Pattern::Binding(at.binding.as_ref().clone()),
                    value,
                )?;
                self.compile_match_pattern_branch(
                    environment,
                    &at.pattern,
                    value,
                    value_type,
                    success,
                    failure,
                )?;
            }
            Pattern::Binding(binding)
                if self
                    .typed_module
                    .resolved()
                    .type_for_pattern(binding.syntax.id)
                    .is_some() =>
            {
                let selected_id = self
                    .typed_module
                    .resolved()
                    .type_for_pattern(binding.syntax.id)
                    .expect("resolved singleton pattern");
                match value_type {
                    CheckedType::Sum(sum) => {
                        let index = sum
                            .alternatives
                            .iter()
                            .position(|alternative| {
                                matches!(alternative, CheckedType::Distinct { id, .. } if *id == selected_id)
                            })
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    binding.syntax.span.clone(),
                                    "singleton pattern does not select a sum alternative",
                                )
                            })?;
                        let BasicValueEnum::StructValue(sum_value) = value else {
                            return Err(Diagnostic::new(
                                binding.syntax.span.clone(),
                                "sum match value has an invalid representation",
                            ));
                        };
                        let tag = self
                            .builder
                            .build_extract_value(sum_value, 0, "match.tag")
                            .map_err(compiler_diagnostic)?
                            .into_int_value();
                        let matches = self
                            .builder
                            .build_int_compare(
                                inkwell::IntPredicate::EQ,
                                tag,
                                self.context.i32_type().const_int(index as u64, false),
                                "match.singleton.tag",
                            )
                            .map_err(compiler_diagnostic)?;
                        self.builder
                            .build_conditional_branch(matches, success, failure)
                            .map_err(compiler_diagnostic)?;
                    }
                    CheckedType::Distinct { id, .. } if *id == selected_id => {
                        self.builder
                            .build_unconditional_branch(success)
                            .map_err(compiler_diagnostic)?;
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            binding.syntax.span.clone(),
                            "checked singleton pattern has an incompatible value",
                        ));
                    }
                }
            }
            Pattern::Binding(binding) if matches!(value_type, CheckedType::Sum(_)) => {
                let selected = self
                    .typed_module
                    .type_of_pattern(binding.syntax.id)
                    .cloned()
                    .map(|ty| substitute_type(ty, &self.active_type_substitutions))
                    .ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "untyped match binding")
                    })?;
                if &selected == value_type {
                    self.bind_pattern_value(environment, pattern, value)?;
                    self.builder
                        .build_unconditional_branch(success)
                        .map_err(compiler_diagnostic)?;
                    return Ok(());
                }
                let CheckedType::Sum(sum) = value_type else {
                    unreachable!()
                };
                let index = sum
                    .alternatives
                    .iter()
                    .position(|alternative| alternative == &selected)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            binding.syntax.span.clone(),
                            "typed match pattern does not select a sum alternative",
                        )
                    })?;
                let BasicValueEnum::StructValue(sum_value) = value else {
                    return Err(Diagnostic::new(
                        binding.syntax.span.clone(),
                        "sum match value has an invalid representation",
                    ));
                };
                let tag = self
                    .builder
                    .build_extract_value(sum_value, 0, "match.tag")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let selected_block = self.context.append_basic_block(
                    success.get_parent().expect("match function"),
                    "match.typed.selected",
                );
                let matches = self
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::EQ,
                        tag,
                        self.context.i32_type().const_int(index as u64, false),
                        "match.typed.tag",
                    )
                    .map_err(compiler_diagnostic)?;
                self.builder
                    .build_conditional_branch(matches, selected_block, failure)
                    .map_err(compiler_diagnostic)?;
                self.builder.position_at_end(selected_block);
                let payload = self.extract_sum_alternative(
                    sum_value,
                    sum,
                    index,
                    binding.syntax.span.clone(),
                )?;
                self.bind_pattern_value(environment, pattern, payload)?;
                self.builder
                    .build_unconditional_branch(success)
                    .map_err(compiler_diagnostic)?;
            }
            Pattern::Binding(_) | Pattern::Wildcard(_) => {
                self.bind_pattern_value(environment, pattern, value)?;
                self.builder
                    .build_unconditional_branch(success)
                    .map_err(compiler_diagnostic)?;
            }
            Pattern::StringLiteral(pattern) => {
                let literal = crate::string_literal::decode(&pattern.literal)
                    .map_err(|message| Diagnostic::new(pattern.syntax.span.clone(), message))?;
                match value_type {
                    CheckedType::String | CheckedType::StringLiteralSet(_) => {
                        self.compile_string_literal_pattern_branch(
                            value,
                            &literal,
                            success,
                            failure,
                            pattern.syntax.span.clone(),
                        )?;
                    }
                    CheckedType::Sum(sum) => {
                        let index = sum
                            .alternatives
                            .iter()
                            .position(|alternative| match alternative {
                                CheckedType::String => true,
                                CheckedType::StringLiteralSet(values) => values.contains(&literal),
                                _ => false,
                            })
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    pattern.syntax.span.clone(),
                                    "checked sum has no string literal alternative",
                                )
                            })?;
                        let BasicValueEnum::StructValue(sum_value) = value else {
                            return Err(Diagnostic::new(
                                pattern.syntax.span.clone(),
                                "sum match value has an invalid representation",
                            ));
                        };
                        let tag = self
                            .builder
                            .build_extract_value(sum_value, 0, "match.tag")
                            .map_err(compiler_diagnostic)?
                            .into_int_value();
                        let selected = self.context.append_basic_block(
                            success.get_parent().expect("match function"),
                            "match.string.selected",
                        );
                        let matches = self
                            .builder
                            .build_int_compare(
                                inkwell::IntPredicate::EQ,
                                tag,
                                self.context.i32_type().const_int(index as u64, false),
                                "match.string.tag",
                            )
                            .map_err(compiler_diagnostic)?;
                        self.builder
                            .build_conditional_branch(matches, selected, failure)
                            .map_err(compiler_diagnostic)?;
                        self.builder.position_at_end(selected);
                        let payload = self.extract_sum_alternative(
                            sum_value,
                            sum,
                            index,
                            pattern.syntax.span.clone(),
                        )?;
                        self.compile_string_literal_pattern_branch(
                            payload,
                            &literal,
                            success,
                            failure,
                            pattern.syntax.span.clone(),
                        )?;
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            pattern.syntax.span.clone(),
                            "checked string pattern has an incompatible value",
                        ));
                    }
                }
            }
            Pattern::Product(product) if product.elements.len() == 1 => {
                self.compile_match_pattern_branch(
                    environment,
                    &product.elements[0],
                    value,
                    value_type,
                    success,
                    failure,
                )?;
            }
            Pattern::Product(product) => {
                let CheckedType::Product(product_type) = value_type else {
                    return Err(Diagnostic::new(
                        product.syntax.span.clone(),
                        "checked product pattern has a non-product value",
                    ));
                };
                let BasicValueEnum::StructValue(product_value) = value else {
                    return Err(Diagnostic::new(
                        product.syntax.span.clone(),
                        "product match value has an invalid representation",
                    ));
                };
                if product.elements.is_empty() {
                    self.builder
                        .build_unconditional_branch(success)
                        .map_err(compiler_diagnostic)?;
                    return Ok(());
                }
                for (index, (element_pattern, element_type)) in product
                    .elements
                    .iter()
                    .zip(&product_type.elements)
                    .enumerate()
                {
                    let element = self
                        .builder
                        .build_extract_value(product_value, index as u32, "match.element")
                        .map_err(|error| {
                            Diagnostic::new(product.syntax.span.clone(), error.to_string())
                        })?;
                    let next = if index + 1 == product.elements.len() {
                        success
                    } else {
                        self.context.append_basic_block(
                            success.get_parent().expect("match function"),
                            "match.pattern",
                        )
                    };
                    self.compile_match_pattern_branch(
                        environment,
                        element_pattern,
                        element,
                        &element_type.value_type,
                        next,
                        failure,
                    )?;
                    if next != success {
                        self.builder.position_at_end(next);
                    }
                }
            }
            Pattern::Nominal(pattern) => match value_type {
                CheckedType::String
                    if self
                        .typed_module
                        .resolved()
                        .type_for_pattern(pattern.syntax.id)
                        .is_some_and(|id| {
                            self.typed_module.resolved().builtin_type(id)
                                == Some(crate::BuiltinType::String)
                        }) =>
                {
                    let representation = self
                        .typed_module
                        .string_representation()
                        .cloned()
                        .ok_or_else(|| {
                            Diagnostic::new(
                                pattern.syntax.span.clone(),
                                "standard library String representation was not checked",
                            )
                        })?;
                    self.compile_match_pattern_branch(
                        environment,
                        &pattern.argument,
                        value,
                        &representation,
                        success,
                        failure,
                    )?;
                }
                CheckedType::Ref(payload)
                    if self
                        .typed_module
                        .resolved()
                        .type_for_pattern(pattern.syntax.id)
                        .is_some_and(|id| {
                            self.typed_module.resolved().builtin_type(id)
                                == Some(crate::BuiltinType::Ref)
                        }) =>
                {
                    let payload_value =
                        self.load_ref_payload(value, payload, pattern.syntax.span.clone())?;
                    self.compile_match_pattern_branch(
                        environment,
                        &pattern.argument,
                        payload_value,
                        payload,
                        success,
                        failure,
                    )?;
                }
                CheckedType::Sum(sum) => {
                    let expected_id = self
                        .typed_module
                        .resolved()
                        .type_for_pattern(pattern.syntax.id)
                        .ok_or_else(|| {
                            Diagnostic::new(pattern.syntax.span.clone(), "unresolved match pattern")
                        })?;
                    let index = sum
                        .alternatives
                        .iter()
                        .position(|alternative| matches!(alternative, CheckedType::Distinct { id, .. } if *id == expected_id))
                        .ok_or_else(|| {
                            Diagnostic::new(
                                pattern.syntax.span.clone(),
                                "match pattern does not select a sum alternative",
                            )
                        })?;
                    let BasicValueEnum::StructValue(sum_value) = value else {
                        return Err(Diagnostic::new(
                            pattern.syntax.span.clone(),
                            "sum match value has an invalid representation",
                        ));
                    };
                    let tag = self
                        .builder
                        .build_extract_value(sum_value, 0, "match.tag")
                        .map_err(|error| {
                            Diagnostic::new(pattern.syntax.span.clone(), error.to_string())
                        })?
                        .into_int_value();
                    let selected = self.context.append_basic_block(
                        success.get_parent().expect("match function"),
                        "match.selected",
                    );
                    let matches = self
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::EQ,
                            tag,
                            self.context.i32_type().const_int(index as u64, false),
                            "match.tag.matches",
                        )
                        .map_err(compiler_diagnostic)?;
                    self.builder
                        .build_conditional_branch(matches, selected, failure)
                        .map_err(compiler_diagnostic)?;
                    self.builder.position_at_end(selected);
                    let payload = self.extract_sum_alternative(
                        sum_value,
                        sum,
                        index,
                        pattern.syntax.span.clone(),
                    )?;
                    let CheckedType::Distinct { representation, .. } = &sum.alternatives[index]
                    else {
                        unreachable!("checked sum alternative");
                    };
                    self.compile_match_pattern_branch(
                        environment,
                        &pattern.argument,
                        payload,
                        representation,
                        success,
                        failure,
                    )?;
                }
                CheckedType::Distinct {
                    id, representation, ..
                } if self
                    .typed_module
                    .resolved()
                    .type_for_pattern(pattern.syntax.id)
                    == Some(*id) =>
                {
                    self.compile_match_pattern_branch(
                        environment,
                        &pattern.argument,
                        value,
                        representation,
                        success,
                        failure,
                    )?;
                }
                _ => {
                    return Err(Diagnostic::new(
                        pattern.syntax.span.clone(),
                        "checked nominal pattern has an incompatible value",
                    ));
                }
            },
            Pattern::Splice(splice) => {
                return Err(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "unexpanded pattern splice",
                ));
            }
        }
        Ok(())
    }

    fn compile_string_literal_pattern_branch(
        &mut self,
        value: BasicValueEnum<'context>,
        literal: &str,
        success: inkwell::basic_block::BasicBlock<'context>,
        failure: inkwell::basic_block::BasicBlock<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let BasicValueEnum::StructValue(string) = value else {
            return Err(Diagnostic::new(
                span,
                "string match value has an invalid representation",
            ));
        };
        let pointer = self
            .builder
            .build_extract_value(string, 0, "match.string.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let length = self
            .builder
            .build_extract_value(string, 1, "match.string.length")
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
        let memcmp = self
            .llvm_module
            .get_function("memcmp")
            .unwrap_or_else(|| self.llvm_module.add_function("memcmp", memcmp_type, None));
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

    fn coerce_sum_value(
        &mut self,
        value: AnyValueEnum<'context>,
        source: &CheckedType,
        target: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let CheckedType::Sum(target_sum) = target else {
            return Err(Diagnostic::new(span, "invalid sum coercion target"));
        };
        let target_type = self.compile_sum_type(target_sum)?;
        let target_slot = self
            .builder
            .build_alloca(target_type, "sum.target")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_store(target_slot, target_type.const_zero())
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let target_tag = self
            .builder
            .build_struct_gep(target_type, target_slot, 0, "sum.target.tag")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let target_payload = self
            .builder
            .build_struct_gep(target_type, target_slot, 1, "sum.target.payload")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let target_payload_type = target_type
            .get_field_type_at_index(1)
            .expect("sum payload field");
        let target_alignment = self.target_data.get_abi_alignment(&target_payload_type);
        let storage = SumStorage {
            tag: target_tag,
            payload: target_payload,
            alignment: target_alignment,
        };

        match source {
            CheckedType::Sum(source_sum) => {
                let source_value = value_as_basic(value)
                    .and_then(|value| match value {
                        BasicValueEnum::StructValue(value) => Some(value),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "sum value has an invalid representation")
                    })?;
                let source_tag = self
                    .builder
                    .build_extract_value(source_value, 0, "sum.source.tag")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                    .into_int_value();
                let function = self
                    .builder
                    .get_insert_block()
                    .and_then(|block| block.get_parent())
                    .expect("sum coercion is in a function");
                let merge = self.context.append_basic_block(function, "sum.coerce.done");
                let cases = source_sum
                    .alternatives
                    .iter()
                    .enumerate()
                    .map(|(index, _)| {
                        (
                            self.context.i32_type().const_int(index as u64, false),
                            self.context.append_basic_block(function, "sum.coerce.case"),
                        )
                    })
                    .collect::<Vec<_>>();
                self.builder
                    .build_switch(source_tag, merge, &cases)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                for (source_index, alternative) in source_sum.alternatives.iter().enumerate() {
                    self.builder.position_at_end(cases[source_index].1);
                    let Some(target_index) =
                        select_sum_alternative(alternative, &target_sum.alternatives)
                            .ok()
                            .flatten()
                    else {
                        // Propagating bindings narrow away their selected success tag before
                        // widening the residual variants into the function result.
                        self.builder
                            .build_unreachable()
                            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                        continue;
                    };
                    let payload = self.extract_sum_alternative(
                        source_value,
                        source_sum,
                        source_index,
                        span.clone(),
                    )?;
                    let target_alternative = &target_sum.alternatives[target_index];
                    let payload = self.coerce_value(
                        payload.as_any_value_enum(),
                        alternative,
                        target_alternative,
                        span.clone(),
                    )?;
                    self.store_sum_payload(
                        payload,
                        target_alternative,
                        target_index,
                        &storage,
                        span.clone(),
                    )?;
                    self.builder
                        .build_unconditional_branch(merge)
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                }
                self.builder.position_at_end(merge);
            }
            _ => {
                let index = select_sum_alternative(source, &target_sum.alternatives)
                    .ok()
                    .flatten()
                    .ok_or_else(|| {
                        Diagnostic::new(
                            span.clone(),
                            "sum injection target is missing a unique source alternative",
                        )
                    })?;
                let target_alternative = &target_sum.alternatives[index];
                let value = self.coerce_value(value, source, target_alternative, span.clone())?;
                self.store_sum_payload(value, target_alternative, index, &storage, span.clone())?;
            }
        }
        self.builder
            .build_load(target_type, target_slot, "sum.value")
            .map(|value| value.as_any_value_enum())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    fn store_sum_payload(
        &mut self,
        value: AnyValueEnum<'context>,
        value_type: &CheckedType,
        index: usize,
        storage: &SumStorage<'context>,
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
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        Ok(())
    }

    fn coerce_value(
        &mut self,
        value: AnyValueEnum<'context>,
        source: &CheckedType,
        target: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if source == target
            || matches!(
                (source, target),
                (CheckedType::StringLiteralSet(_), CheckedType::String)
                    | (
                        CheckedType::StringLiteralSet(_),
                        CheckedType::StringLiteralSet(_)
                    )
                    | (CheckedType::NumberLiteral(_), CheckedType::USize)
            )
        {
            Ok(value)
        } else if matches!(
            (source, target),
            (CheckedType::Ref(_), CheckedType::Slice(_))
        ) {
            self.coerce_slice_ref_value(value, source, target, span)
        } else if matches!(target, CheckedType::Sum(_)) {
            self.coerce_sum_value(value, source, target, span)
        } else {
            Err(Diagnostic::new(
                span,
                format!("unsupported runtime coercion from `{source}` to `{target}`"),
            ))
        }
    }

    fn extract_sum_alternative(
        &mut self,
        value: inkwell::values::StructValue<'context>,
        sum: &crate::CheckedSumType,
        index: usize,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
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

    fn compile_c_string_macro(
        &mut self,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let Expression::String(string) = call.argument.as_ref() else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "`c_string` requires a string literal",
            ));
        };
        let value = crate::string_literal::decode(&string.literal)
            .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
        if value.as_bytes().contains(&0) {
            return Err(Diagnostic::new(
                string.syntax.span.clone(),
                "C string literals cannot contain an interior NUL byte",
            ));
        }
        self.build_owned_c_string(&value, string.syntax.span.clone())
    }

    fn compile_c_string_literal(
        &mut self,
        string: &crate::CStringExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let value = crate::string_literal::decode(&string.literal)
            .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
        if value.as_bytes().contains(&0) {
            return Err(Diagnostic::new(
                string.syntax.span.clone(),
                "C string literals cannot contain an interior NUL byte",
            ));
        }
        self.build_owned_c_string(&value, string.syntax.span.clone())
    }

    fn build_owned_c_string(
        &mut self,
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

    fn build_string_value(
        &mut self,
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

    fn build_ref_value(
        &mut self,
        value: BasicValueEnum<'context>,
        payload: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        let payload_type = self.compile_type(payload)?;
        let size = self.target_data.get_store_size(&payload_type);
        let pointer = self.build_gc_allocation(
            self.size_type.const_int(size, false),
            "ref.allocate",
            span.clone(),
        )?;
        self.builder
            .build_store(pointer, value)
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        if self.typed_module.type_needs_drop(payload) {
            let finalizer = self.ensure_gc_finalizer(payload)?;
            self.set_gc_finalizer(pointer, finalizer)?;
        }
        Ok(pointer)
    }

    fn ensure_gc_finalizer(
        &mut self,
        payload: &CheckedType,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let key = format!("{payload:?}");
        if let Some(function) = self.gc_finalizers.get(&key).copied() {
            return Ok(function);
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let name = format!("__staple_gc_finalize_{:016x}", hasher.finish());
        let function_type = self.context.void_type().fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let function = self.llvm_module.add_function(&name, function_type, None);
        self.gc_finalizers.insert(key, function);

        let previous_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);
        let pointer = function
            .get_first_param()
            .expect("finalizer payload")
            .into_pointer_value();
        let payload_type = self.compile_type(payload)?;
        let value = self
            .builder
            .build_load(payload_type, pointer, "finalizer.value")
            .map_err(compiler_diagnostic)?;
        self.compile_drop_value(value, payload, Span::Compiler)?;
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = previous_block {
            self.builder.position_at_end(block);
        }
        Ok(function)
    }

    fn ensure_cell_finalizer(
        &mut self,
        value_type: &CheckedType,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let key = format!("cell:{value_type:?}");
        if let Some(function) = self.gc_finalizers.get(&key).copied() {
            return Ok(function);
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let name = format!("__staple_gc_finalize_cell_{:016x}", hasher.finish());
        let function_type = self.context.void_type().fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let function = self.llvm_module.add_function(&name, function_type, None);
        self.gc_finalizers.insert(key, function);
        let previous_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);
        let cell = function
            .get_first_param()
            .expect("cell payload")
            .into_pointer_value();
        self.compile_conditional_cell_drop(cell, value_type, Span::Compiler)?;
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = previous_block {
            self.builder.position_at_end(block);
        }
        Ok(function)
    }

    fn compile_conditional_cell_drop(
        &mut self,
        cell: inkwell::values::PointerValue<'context>,
        value_type: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let llvm_value_type = self.compile_type(value_type)?;
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
            .ok_or_else(|| Diagnostic::new(span.clone(), "cell drop outside a function"))?;
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
        self.compile_drop_value(value, value_type, span)?;
        self.builder
            .build_store(state, self.context.i8_type().const_zero())
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(continue_block)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(continue_block);
        Ok(())
    }

    fn set_gc_finalizer(
        &mut self,
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

    fn build_gc_allocation(
        &mut self,
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

    fn load_ref_payload(
        &self,
        value: BasicValueEnum<'context>,
        payload: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let BasicValueEnum::PointerValue(pointer) = value else {
            return Err(Diagnostic::new(
                span,
                "Ref value has an invalid representation",
            ));
        };
        let payload_type = self.compile_type(payload)?;
        self.builder
            .build_load(payload_type, pointer, "ref.payload")
            .map_err(compiler_diagnostic)
    }

    fn slice_type(&self) -> inkwell::types::StructType<'context> {
        self.context.struct_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.size_type.into(),
            ],
            false,
        )
    }

    fn coerce_slice_ref_value(
        &self,
        value: AnyValueEnum<'context>,
        source: &CheckedType,
        target: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let length = slice_ref_length(source, target)
            .ok_or_else(|| Diagnostic::new(span.clone(), "invalid slice coercion"))?;
        let BasicValueEnum::PointerValue(pointer) = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(span.clone(), "invalid fixed reference representation")
        })?
        else {
            return Err(Diagnostic::new(
                span,
                "invalid fixed reference representation",
            ));
        };
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

    fn compile_index_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        index: &crate::IndexExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let dispatch = self
            .typed_module
            .trait_dispatch_for(index.syntax.id)
            .cloned()
            .ok_or_else(|| Diagnostic::new(index.syntax.span.clone(), "missing Index dispatch"))?;
        let trait_id = self
            .typed_module
            .resolved()
            .trait_for_method(dispatch.method)
            .expect("Index method owner");
        let arguments = dispatch
            .arguments
            .into_iter()
            .map(|argument| substitute_type(argument, &self.active_type_substitutions))
            .collect::<Vec<_>>();
        let arguments = self
            .typed_module
            .complete_trait_arguments(trait_id, &arguments)
            .ok_or_else(|| {
                Diagnostic::new(index.syntax.span.clone(), "incomplete Index dispatch")
            })?;
        let function = self.trait_method_code(
            trait_id,
            &arguments,
            dispatch.method,
            index.syntax.span.clone(),
        )?;
        let function_type = self
            .typed_module
            .instantiated_trait_method_type(trait_id, &arguments, dispatch.method)
            .ok_or_else(|| Diagnostic::new(index.syntax.span.clone(), "unchecked Index call"))?;
        let target = self.compile_expression(environment, &index.value)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let position = self.compile_expression(environment, &index.index)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let target = value_as_basic(target).ok_or_else(|| {
            Diagnostic::new(
                index.value.syntax().span.clone(),
                "indexed value is not first-class",
            )
        })?;
        let position = value_as_basic(position).ok_or_else(|| {
            Diagnostic::new(
                index.index.syntax().span.clone(),
                "index is not first-class",
            )
        })?;
        let mut call_arguments = self.compile_resource_arguments(
            environment,
            &function_type.effects,
            index.syntax.span.clone(),
        )?;
        call_arguments.insert(
            0,
            self.context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
        );
        call_arguments.push(target.into());
        call_arguments.push(position.into());
        self.builder
            .build_direct_call(function, &call_arguments, "index.call")
            .map_err(|error| Diagnostic::new(index.syntax.span.clone(), error.to_string()))?
            .try_as_basic_value()
            .basic()
            .map(AnyValueEnum::from)
            .ok_or_else(|| {
                Diagnostic::new(index.syntax.span.clone(), "Index result is not first-class")
            })
    }

    fn compile_index_load(
        &mut self,
        pointer: inkwell::values::PointerValue<'context>,
        position: inkwell::values::IntValue<'context>,
        length: inkwell::values::IntValue<'context>,
        element: CheckedType,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let out_of_bounds = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGE,
                position,
                length,
                "index.out_of_bounds",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(out_of_bounds, span.clone())?;
        let element_type = self.compile_type(&element)?;
        let pointer = unsafe {
            self.builder
                .build_gep(element_type, pointer, &[position], "index.element")
        }
        .map_err(compiler_diagnostic)?;
        self.builder
            .build_load(element_type, pointer, "index.value")
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    fn compile_symbol_value(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        check_initialization: bool,
        span: Span,
        unavailable: String,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if let Some(pointer) = environment.parameter_pointers.get(&symbol).copied() {
            let value_type = self
                .typed_module
                .type_of_symbol(symbol)
                .cloned()
                .map(|ty| substitute_type(ty, &self.active_type_substitutions))
                .ok_or_else(|| Diagnostic::new(span.clone(), "unchecked parameter"))?;
            return self
                .builder
                .build_load(self.compile_type(&value_type)?, pointer, "parameter")
                .map(|value| value.as_any_value_enum())
                .map_err(|error| Diagnostic::new(span, error.to_string()));
        }
        if let Some(value) = environment.locals.get(&symbol).copied() {
            return Ok(value);
        }
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let cell_type = self.compile_binding_cell_type(symbol)?;
            self.force_derived_read(environment, symbol, span.clone())?;
            let state = self
                .builder
                .build_struct_gep(cell_type, cell, 1, "binding.state")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            if check_initialization {
                self.build_initialization_check(state, span.clone())?;
            }
            let value_slot = self
                .builder
                .build_struct_gep(cell_type, cell, 0, "binding.value")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            let value_type = self
                .typed_module
                .type_of_symbol(symbol)
                .cloned()
                .ok_or_else(|| Diagnostic::new(span.clone(), "unchecked local binding"))?;
            let value_type = substitute_type(value_type, &self.active_type_substitutions);
            let llvm_type = self.compile_type(&value_type)?;
            self.track_signal_read(environment, symbol, span.clone())?;
            return self
                .builder
                .build_load(llvm_type, value_slot, "binding")
                .map(|value| value.as_any_value_enum())
                .map_err(|error| Diagnostic::new(span, error.to_string()));
        }
        if let Some(code) = self.closure_codes.get(&symbol).copied() {
            let closure_environment =
                if self.function_symbols.get(&symbol).copied() == environment.function_id {
                    environment.closure_environment.unwrap_or_else(|| {
                        self.context.ptr_type(AddressSpace::default()).const_null()
                    })
                } else {
                    self.context.ptr_type(AddressSpace::default()).const_null()
                };
            return self
                .build_closure_value(code, closure_environment)
                .map(|value| value.as_any_value_enum());
        }
        if self.external_symbols.contains(&symbol) {
            return Err(Diagnostic::new(
                span,
                "variadic external functions cannot be used as first-class values",
            ));
        }
        if let Some(global) = self.storage.get(&symbol).copied() {
            self.force_derived_read(environment, symbol, span.clone())?;
            if check_initialization
                && let Some(state) = self.initialization_states.get(&symbol).copied()
            {
                self.build_initialization_check(state.as_pointer_value(), span.clone())?;
            }
            let value_type = self
                .typed_module
                .type_of_symbol(symbol)
                .ok_or_else(|| Diagnostic::new(span.clone(), "unchecked global value"))?;
            let llvm_type = self.compile_type(value_type)?;
            self.track_signal_read(environment, symbol, span.clone())?;
            return self
                .builder
                .build_load(llvm_type, global.as_pointer_value(), "global")
                .map(|value| value.as_any_value_enum())
                .map_err(|error| Diagnostic::new(span, error.to_string()));
        }
        Err(Diagnostic::new(span, unavailable))
    }

    fn signal_metadata_value(
        &self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: Span,
    ) -> CodeGenerationResult<Option<inkwell::values::PointerValue<'context>>> {
        if !self.typed_module.resolved().is_signal_symbol(symbol) {
            return Ok(None);
        }
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let slot = if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            self.builder
                .build_struct_gep(
                    self.compile_binding_cell_type(symbol)?,
                    cell,
                    2,
                    "signal.metadata",
                )
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        } else if let Some(global) = self.signal_metadata.get(&symbol).copied() {
            global.as_pointer_value()
        } else {
            return Err(Diagnostic::new(span, "signal metadata is unavailable"));
        };
        self.builder
            .build_load(pointer_type, slot, "signal")
            .map(|value| Some(value.into_pointer_value()))
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    fn track_signal_read(
        &self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let Some(signal) = self.signal_metadata_value(environment, symbol, span.clone())? else {
            return Ok(());
        };
        self.build_reactive_runtime_call(
            "__staple_signal_track",
            &[signal.into()],
            None,
            "signal.track",
            span,
        )?;
        Ok(())
    }

    fn force_derived_read(
        &self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: Span,
    ) -> CodeGenerationResult<()> {
        if !self.typed_module.is_derived_symbol(symbol) {
            return Ok(());
        }
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let slot = if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            self.builder
                .build_struct_gep(
                    self.compile_binding_cell_type(symbol)?,
                    cell,
                    2,
                    "derived.metadata",
                )
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        } else if let Some(global) = self.derived_metadata.get(&symbol).copied() {
            global.as_pointer_value()
        } else {
            return Err(Diagnostic::new(span, "derived metadata is unavailable"));
        };
        let derived = self
            .builder
            .build_load(pointer_type, slot, "derived")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        self.build_reactive_runtime_call(
            "__staple_derived_read",
            &[derived.into()],
            None,
            "derived.read",
            Span::Compiler,
        )?;
        Ok(())
    }

    fn dispose_reactive_scopes(
        &self,
        environment: &FunctionEnvironment<'context>,
        keep: usize,
        span: Span,
    ) -> CodeGenerationResult<()> {
        for scope in environment.reactive_scopes[keep..].iter().rev() {
            self.build_reactive_runtime_call(
                "__staple_reactive_scope_dispose",
                &[(*scope).into()],
                None,
                "reactive.dispose",
                span.clone(),
            )?;
        }
        Ok(())
    }

    fn build_initialization_check(
        &mut self,
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

    fn check_symbol_initialization(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: Span,
    ) -> CodeGenerationResult<()> {
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let generic = self
                .typed_module
                .type_of_symbol(symbol)
                .cloned()
                .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))
                .is_some_and(|value_type| contains_type_parameter(&value_type));
            let state = if generic {
                cell
            } else {
                let cell_type = self.compile_binding_cell_type(symbol)?;
                self.builder
                    .build_struct_gep(cell_type, cell, 1, "binding.state")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            };
            return self.build_initialization_check(state, span);
        }
        if let Some(state) = self.initialization_states.get(&symbol).copied() {
            return self.build_initialization_check(state.as_pointer_value(), span);
        }
        Ok(())
    }

    fn compile_call_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if let Some(plan) = self
            .typed_module
            .juxtaposed_call_plan(call.syntax.id)
            .cloned()
        {
            let expected = match plan.function.parameter.as_ref() {
                CheckedType::Product(product) => product.elements.len(),
                _ => 0,
            };
            if plan.arguments.len() == expected {
                let mut callee = call.callee.as_ref();
                for _ in 1..plan.consumed_calls {
                    let Expression::Call(previous) = callee else {
                        break;
                    };
                    callee = previous.callee.as_ref();
                }
                let callee = self.compile_expression(environment, callee)?;
                let argument = Expression::Product(ProductExpression {
                    syntax: call.syntax.clone(),
                    elements: plan
                        .arguments
                        .into_iter()
                        .map(|value| crate::ProductElement {
                            syntax: value.syntax().clone(),
                            name: None,
                            designated: false,
                            value,
                            spread: false,
                            named_spread: false,
                        })
                        .collect(),
                });
                return self.compile_indirect_call_value(
                    environment,
                    callee,
                    &argument,
                    &plan.function,
                    call.syntax.span.clone(),
                );
            }
        }
        if let Some(plan) = self
            .typed_module
            .curried_default_plan(call.syntax.id)
            .cloned()
        {
            let mut callee = self.compile_expression(environment, &call.callee)?;
            if environment.did_return {
                return Ok(self.unit_value());
            }
            for default in &plan.defaults {
                callee = self.compile_indirect_call_value(
                    environment,
                    callee,
                    &default.value,
                    &default.function,
                    call.syntax.span.clone(),
                )?;
                if environment.did_return {
                    return Ok(self.unit_value());
                }
            }
            if matches!(call.argument.as_ref(), Expression::Name(name) if name.name == "_") {
                return Ok(callee);
            }
            let function = plan
                .defaults
                .last()
                .and_then(|default| match default.function.result.as_ref() {
                    CheckedType::Function(function) => Some(function.clone()),
                    _ => None,
                })
                .expect("a defaulted curried arrow has a residual function");
            return self.compile_indirect_call_value(
                environment,
                callee,
                &call.argument,
                &function,
                call.syntax.span.clone(),
            );
        }
        if let Some(dispatch) = self
            .typed_module
            .trait_dispatch_for(call.callee.syntax().id)
            .cloned()
        {
            let trait_id = self
                .typed_module
                .resolved()
                .trait_for_method(dispatch.method)
                .expect("trait method owner");
            let arguments = dispatch
                .arguments
                .into_iter()
                .map(|argument| substitute_type(argument, &self.active_type_substitutions))
                .collect::<Vec<_>>();
            let arguments = self
                .typed_module
                .complete_trait_arguments(trait_id, &arguments)
                .ok_or_else(|| {
                    Diagnostic::new(
                        call.callee.syntax().span.clone(),
                        "could not infer functional dependency arguments",
                    )
                })?;
            let function = self.trait_method_code(
                trait_id,
                &arguments,
                dispatch.method,
                call.callee.syntax().span.clone(),
            )?;
            let function_type = self
                .typed_module
                .instantiated_trait_method_type(trait_id, &arguments, dispatch.method)
                .ok_or_else(|| Diagnostic::new(call.syntax.span.clone(), "unchecked trait call"))?;
            let compiled =
                self.compile_effect_arguments(environment, &call.argument, &function_type)?;
            let mut arguments = compiled.values;
            if environment.did_return {
                return Ok(self.unit_value());
            }
            let mut hidden = self.compile_resource_arguments(
                environment,
                &function_type.effects,
                call.syntax.span.clone(),
            )?;
            hidden.append(&mut arguments);
            arguments = hidden;
            arguments.insert(
                0,
                self.context
                    .ptr_type(AddressSpace::default())
                    .const_null()
                    .into(),
            );
            let call_site = self
                .builder
                .build_direct_call(function, &arguments, "trait.call")
                .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
            self.drop_mutation_temporaries(compiled.temporaries, call.syntax.span.clone())?;
            return Ok(call_site
                .try_as_basic_value()
                .unwrap_basic()
                .as_any_value_enum());
        }
        if let Some(symbol) = self.typed_module.symbol_for(call.callee.syntax().id)
            && let Some(intrinsic) = self.typed_module.resolved().intrinsic_function(symbol)
        {
            return self.compile_intrinsic_call(environment, call, intrinsic);
        }
        if let Some(symbol) = self.typed_module.symbol_for(call.callee.syntax().id)
            && !environment.locals.contains_key(&symbol)
            && let Some(function_id) = self.function_symbols.get(&symbol).copied()
            && self
                .typed_module
                .functions()
                .iter()
                .find(|function| function.id == function_id)
                .is_some_and(|function| function.captures.is_empty())
            && contains_type_parameter(&CheckedType::Function(
                self.typed_module
                    .type_of_function(function_id)
                    .expect("checked function")
                    .clone(),
            ))
        {
            if self
                .typed_module
                .resolved()
                .requires_initialization_check(call.callee.syntax().id)
            {
                self.check_symbol_initialization(
                    environment,
                    symbol,
                    call.callee.syntax().span.clone(),
                )?;
            }
            let Some(CheckedType::Function(function_type)) =
                self.concrete_expression_type(&call.callee)
            else {
                return Err(Diagnostic::new(
                    call.callee.syntax().span.clone(),
                    "generic call has no concrete function type",
                ));
            };
            let function = self.ensure_function_specialization(function_id, &function_type)?;
            let compiled =
                self.compile_effect_arguments(environment, &call.argument, &function_type)?;
            let mut arguments = compiled.values;
            if environment.did_return {
                return Ok(self.unit_value());
            }
            let closure_environment = if environment.function_id == Some(function_id) {
                environment
                    .closure_environment
                    .unwrap_or_else(|| self.context.ptr_type(AddressSpace::default()).const_null())
            } else {
                self.context.ptr_type(AddressSpace::default()).const_null()
            };
            let mut hidden = self.compile_resource_arguments(
                environment,
                &function_type.effects,
                call.syntax.span.clone(),
            )?;
            hidden.append(&mut arguments);
            arguments = hidden;
            arguments.insert(0, closure_environment.into());
            let call_site = self
                .builder
                .build_direct_call(function, &arguments, "generic.call")
                .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
            self.drop_mutation_temporaries(compiled.temporaries, call.syntax.span.clone())?;
            return Ok(call_site
                .try_as_basic_value()
                .unwrap_basic()
                .as_any_value_enum());
        }
        if let Some(symbol) = self.typed_module.symbol_for(call.callee.syntax().id)
            && !environment.locals.contains_key(&symbol)
            && let Some(AnyValueEnum::FunctionValue(function)) = self.globals.get(&symbol).copied()
        {
            let internal = !self.external_symbols.contains(&symbol);
            let function_type = match self.concrete_expression_type(&call.callee) {
                Some(CheckedType::Function(function)) => Some(function),
                _ => None,
            };
            let scoped_c_string_temporary = !internal
                && self
                    .concrete_expression_type(&call.argument)
                    .is_some_and(|ty| ty == CheckedType::CString)
                && self
                    .typed_module
                    .symbol_for(call.argument.syntax().id)
                    .is_none();
            let expected_count = function_type
                .as_ref()
                .map(|function| self.compile_parameter_types(&function.parameter))
                .transpose()?
                .map_or(
                    function.count_params() as usize - usize::from(internal),
                    |types| types.len(),
                );
            let compiled = if internal {
                function_type
                    .as_ref()
                    .map(|ty| self.compile_effect_arguments(environment, &call.argument, ty))
                    .transpose()?
            } else {
                None
            };
            let mut arguments = if let Some(compiled) = &compiled {
                compiled.values.clone()
            } else {
                self.compile_arguments(
                    environment,
                    &call.argument,
                    expected_count,
                    function.get_type().is_var_arg(),
                )?
            };
            if environment.did_return {
                return Ok(self.unit_value());
            }
            if internal {
                let closure_environment =
                    if self.function_symbols.get(&symbol).copied() == environment.function_id {
                        environment.closure_environment.unwrap_or_else(|| {
                            self.context.ptr_type(AddressSpace::default()).const_null()
                        })
                    } else {
                        self.context.ptr_type(AddressSpace::default()).const_null()
                    };
                let empty_resources = CheckedEffectSet::default();
                let resources = function_type
                    .as_ref()
                    .map(|function| &function.effects)
                    .unwrap_or(&empty_resources);
                let mut hidden = self.compile_resource_arguments(
                    environment,
                    resources,
                    call.syntax.span.clone(),
                )?;
                hidden.append(&mut arguments);
                arguments = hidden;
                arguments.insert(0, closure_environment.into());
            }
            let call_site = self
                .builder
                .build_direct_call(function, &arguments, "call")
                .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
            if let Some(compiled) = compiled {
                self.drop_mutation_temporaries(compiled.temporaries, call.syntax.span.clone())?;
            }
            if scoped_c_string_temporary
                && let Some(inkwell::values::BasicMetadataValueEnum::PointerValue(pointer)) =
                    arguments.first().copied()
            {
                self.compile_drop_value(
                    pointer.into(),
                    &CheckedType::CString,
                    call.syntax.span.clone(),
                )?;
            }
            return Ok(call_site
                .try_as_basic_value()
                .unwrap_basic()
                .as_any_value_enum());
        }

        let callee = self.compile_expression(environment, &call.callee)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let AnyValueEnum::StructValue(closure) = callee else {
            return Err(Diagnostic::new(
                call.callee.syntax().span.clone(),
                "expression is not a closure",
            ));
        };
        let Some(CheckedType::Function(function_type)) =
            self.concrete_expression_type(&call.callee)
        else {
            return Err(Diagnostic::new(
                call.callee.syntax().span.clone(),
                "called expression has no function type",
            ));
        };
        let compiled =
            self.compile_effect_arguments(environment, &call.argument, &function_type)?;
        let mut arguments = compiled.values;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let code = self
            .builder
            .build_extract_value(closure, 0, "closure.code")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(closure, 1, "closure.environment")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?
            .into_pointer_value();
        let mut hidden = self.compile_resource_arguments(
            environment,
            &function_type.effects,
            call.syntax.span.clone(),
        )?;
        hidden.append(&mut arguments);
        arguments = hidden;
        arguments.insert(0, closure_environment.into());
        let call_site = self
            .builder
            .build_indirect_call(
                self.compile_closure_function_type(&function_type)?,
                code,
                &arguments,
                "closure.call",
            )
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.drop_mutation_temporaries(compiled.temporaries, call.syntax.span.clone())?;
        Ok(call_site
            .try_as_basic_value()
            .unwrap_basic()
            .as_any_value_enum())
    }

    fn compile_indirect_call_value(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        callee: AnyValueEnum<'context>,
        argument: &Expression,
        function_type: &CheckedFunctionType,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let AnyValueEnum::StructValue(closure) = callee else {
            return Err(Diagnostic::new(span, "expression is not a closure"));
        };
        let compiled = self.compile_effect_arguments(environment, argument, function_type)?;
        let mut arguments = compiled.values;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let code = self
            .builder
            .build_extract_value(closure, 0, "closure.code")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(closure, 1, "closure.environment")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .into_pointer_value();
        let mut hidden =
            self.compile_resource_arguments(environment, &function_type.effects, span.clone())?;
        hidden.append(&mut arguments);
        hidden.insert(0, closure_environment.into());
        let call_site = self
            .builder
            .build_indirect_call(
                self.compile_closure_function_type(function_type)?,
                code,
                &hidden,
                "closure.defaulted.call",
            )
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.drop_mutation_temporaries(compiled.temporaries, span)?;
        Ok(call_site
            .try_as_basic_value()
            .unwrap_basic()
            .as_any_value_enum())
    }

    fn compile_resource_arguments(
        &self,
        environment: &FunctionEnvironment<'context>,
        resources: &CheckedEffectSet,
        span: Span,
    ) -> CodeGenerationResult<Vec<inkwell::values::BasicMetadataValueEnum<'context>>> {
        resources
            .resources
            .iter()
            .map(|resource| {
                let bound = environment
                    .resources
                    .iter()
                    .rev()
                    .find(|candidate| candidate.resource.value_type == resource.value_type)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            span.clone(),
                            format!("resource `{}` is not available", resource.value_type),
                        )
                    })?;
                let value = if resource.mutable
                    || !self
                        .typed_module
                        .is_copy_in_function(&resource.value_type, environment.function_id)
                {
                    if bound.indirect {
                        bound.value
                    } else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            format!("resource `{}` is not borrowable", resource.value_type),
                        ));
                    }
                } else if bound.indirect {
                    let pointer = value_as_basic(bound.value)
                        .expect("borrowed resource pointer")
                        .into_pointer_value();
                    self.builder
                        .build_load(
                            self.compile_type(&resource.value_type)?,
                            pointer,
                            "resource.copy",
                        )
                        .map_err(compiler_diagnostic)?
                        .as_any_value_enum()
                } else {
                    bound.value
                };
                value_as_basic(value)
                    .map(Into::into)
                    .ok_or_else(|| Diagnostic::new(span.clone(), "resource is not first-class"))
            })
            .collect()
    }

    fn compile_intrinsic_call(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
        intrinsic: IntrinsicFunction,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        match intrinsic {
            IntrinsicFunction::ReactiveScope => {
                self.compile_expression(environment, &call.argument)?;
                let value = self
                    .build_reactive_runtime_call(
                        "__staple_reactive_scope_create",
                        &[],
                        Some(self.context.ptr_type(AddressSpace::default()).into()),
                        "reactive.scope",
                        call.syntax.span.clone(),
                    )?
                    .expect("reactive_scope_create returns a pointer");
                return Ok(value.as_any_value_enum());
            }
            IntrinsicFunction::Reaction => {
                return self.compile_reaction(environment, call);
            }
            IntrinsicFunction::Batch => {
                return self.compile_batch(environment, call);
            }
            IntrinsicFunction::ToString { value } => {
                return self.compile_numeric_to_string(environment, call, value);
            }
            IntrinsicFunction::StringFromCString => {
                return self.compile_string_from_c_string(environment, call);
            }
            IntrinsicFunction::StringToCString => {
                return self.compile_string_to_c_string(environment, call);
            }
            IntrinsicFunction::StringAdd => {
                return self.compile_string_add(environment, call);
            }
            IntrinsicFunction::BufferWithCapacity => {
                return self.compile_buffer_with_capacity(environment, call);
            }
            IntrinsicFunction::BufferLength => {
                return self.compile_buffer_metadata(environment, call, 0, "buffer.length");
            }
            IntrinsicFunction::BufferCapacity => {
                return self.compile_buffer_metadata(environment, call, 1, "buffer.capacity");
            }
            IntrinsicFunction::BufferPush => {
                return self.compile_buffer_push(environment, call);
            }
            IntrinsicFunction::BufferPop => {
                return self.compile_buffer_pop(environment, call);
            }
            IntrinsicFunction::BufferGet => {
                return self.compile_buffer_get(environment, call);
            }
            IntrinsicFunction::BufferFreeze => {
                return self.compile_buffer_freeze(environment, call);
            }
            IntrinsicFunction::BufferTransfer => {
                return self.compile_buffer_transfer(environment, call);
            }
            IntrinsicFunction::BufferClone => {
                return self.compile_buffer_clone(environment, call);
            }
            IntrinsicFunction::Drop => {
                let value_type = self
                    .concrete_expression_type(&call.argument)
                    .unwrap_or(CheckedType::Error);
                let value = self.compile_expression(environment, &call.argument)?;
                if let Some(value) = value_as_basic(value) {
                    self.compile_drop_value(value, &value_type, call.syntax.span.clone())?;
                }
                return Ok(self.unit_value());
            }
            IntrinsicFunction::Snapshot => {
                let previous = self
                    .build_reactive_runtime_call(
                        "__staple_tracking_suspend",
                        &[],
                        Some(self.context.ptr_type(AddressSpace::default()).into()),
                        "tracking.suspend",
                        call.syntax.span.clone(),
                    )?
                    .expect("tracking_suspend returns a pointer");
                let value = self.compile_expression(environment, &call.argument)?;
                self.build_reactive_runtime_call(
                    "__staple_tracking_restore",
                    &[previous.into()],
                    None,
                    "tracking.restore",
                    call.syntax.span.clone(),
                )?;
                return Ok(value);
            }
            IntrinsicFunction::SliceLength => {
                let value = self.compile_expression(environment, &call.argument)?;
                let Some(BasicValueEnum::StructValue(reference)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        call.argument.syntax().span.clone(),
                        "length requires a slice",
                    ));
                };
                return self
                    .builder
                    .build_extract_value(reference, 1, "slice.length")
                    .map(|value| value.as_any_value_enum())
                    .map_err(compiler_diagnostic);
            }
            IntrinsicFunction::SliceFromRef => {
                let source_type = self
                    .concrete_expression_type(&call.argument)
                    .unwrap_or(CheckedType::Error);
                let target_type = self
                    .concrete_expression_type(&Expression::Call(call.clone()))
                    .ok_or_else(|| {
                        Diagnostic::new(call.syntax.span.clone(), "unchecked from_ref result")
                    })?;
                let value = self.compile_expression(environment, &call.argument)?;
                return self.coerce_slice_ref_value(
                    value,
                    &source_type,
                    &target_type,
                    call.syntax.span.clone(),
                );
            }
            IntrinsicFunction::RefReplace => {
                let arguments = self.compile_arguments(environment, &call.argument, 2, false)?;
                let [
                    inkwell::values::BasicMetadataValueEnum::PointerValue(reference),
                    replacement,
                ] = arguments.as_slice()
                else {
                    return Err(Diagnostic::new(
                        call.argument.syntax().span.clone(),
                        "replace requires a fixed Ref and a replacement value",
                    ));
                };
                let payload = self
                    .concrete_expression_type(&Expression::Call(call.clone()))
                    .ok_or_else(|| {
                        Diagnostic::new(call.syntax.span.clone(), "unchecked replace payload")
                    })?;
                let payload_type = self.compile_type(&payload)?;
                let old = self
                    .builder
                    .build_load(payload_type, *reference, "ref.replace.old")
                    .map_err(compiler_diagnostic)?;
                let replacement = BasicValueEnum::try_from(*replacement).map_err(|_| {
                    Diagnostic::new(
                        call.argument.syntax().span.clone(),
                        "replacement is not storable",
                    )
                })?;
                self.builder
                    .build_store(*reference, replacement)
                    .map_err(compiler_diagnostic)?;
                return Ok(old.as_any_value_enum());
            }
            _ => {}
        }
        let arguments = self.compile_arguments(environment, &call.argument, 2, false)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        if let IntrinsicFunction::FloatBinary { float, operation } = intrinsic {
            let [
                inkwell::values::BasicMetadataValueEnum::FloatValue(left),
                inkwell::values::BasicMetadataValueEnum::FloatValue(right),
            ] = arguments.as_slice()
            else {
                return Err(Diagnostic::new(
                    call.argument.syntax().span.clone(),
                    "float arithmetic intrinsic operands must be floats",
                ));
            };
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
                IntegerBinaryOperation::Add => self.builder.build_float_add(*left, *right, &name),
                IntegerBinaryOperation::Subtract => {
                    self.builder.build_float_sub(*left, *right, &name)
                }
                IntegerBinaryOperation::Multiply => {
                    self.builder.build_float_mul(*left, *right, &name)
                }
                IntegerBinaryOperation::Divide => {
                    self.builder.build_float_div(*left, *right, &name)
                }
            }
            .map_err(compiler_diagnostic)?;
            return Ok(value.as_any_value_enum());
        }
        if let IntrinsicFunction::FloatCompare { float, operation } = intrinsic {
            let [
                inkwell::values::BasicMetadataValueEnum::FloatValue(left),
                inkwell::values::BasicMetadataValueEnum::FloatValue(right),
            ] = arguments.as_slice()
            else {
                return Err(Diagnostic::new(
                    call.argument.syntax().span.clone(),
                    "float comparison intrinsic operands must be floats",
                ));
            };
            let predicate = match operation {
                IntegerCompareOperation::Equal => inkwell::FloatPredicate::OEQ,
                IntegerCompareOperation::NotEqual => inkwell::FloatPredicate::UNE,
                IntegerCompareOperation::LessThan => inkwell::FloatPredicate::OLT,
                IntegerCompareOperation::LessThanOrEqual => inkwell::FloatPredicate::OLE,
                IntegerCompareOperation::GreaterThan => inkwell::FloatPredicate::OGT,
                IntegerCompareOperation::GreaterThanOrEqual => inkwell::FloatPredicate::OGE,
            };
            let condition = self
                .builder
                .build_float_compare(
                    predicate,
                    *left,
                    *right,
                    &format!("{}.compare", float.intrinsic_name()),
                )
                .map_err(compiler_diagnostic)?;
            return self.compile_bool(condition, call.syntax.id, call.syntax.span.clone());
        }
        let [
            inkwell::values::BasicMetadataValueEnum::IntValue(left),
            inkwell::values::BasicMetadataValueEnum::IntValue(right),
        ] = arguments.as_slice()
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "I32 arithmetic intrinsic operands must be integers",
            ));
        };
        let value = match intrinsic {
            IntrinsicFunction::IntegerBinary {
                integer,
                operation: IntegerBinaryOperation::Add,
            } => self.builder.build_int_add(
                *left,
                *right,
                &format!("{}.add", integer.intrinsic_name()),
            ),
            IntrinsicFunction::IntegerBinary {
                integer,
                operation: IntegerBinaryOperation::Subtract,
            } => self.builder.build_int_sub(
                *left,
                *right,
                &format!("{}.subtract", integer.intrinsic_name()),
            ),
            IntrinsicFunction::IntegerBinary {
                integer,
                operation: IntegerBinaryOperation::Multiply,
            } => self.builder.build_int_mul(
                *left,
                *right,
                &format!("{}.multiply", integer.intrinsic_name()),
            ),
            IntrinsicFunction::IntegerBinary {
                integer,
                operation: IntegerBinaryOperation::Divide,
            } if integer.is_signed() => self.builder.build_int_signed_div(
                *left,
                *right,
                &format!("{}.divide", integer.intrinsic_name()),
            ),
            IntrinsicFunction::IntegerBinary {
                integer,
                operation: IntegerBinaryOperation::Divide,
            } => self.builder.build_int_unsigned_div(
                *left,
                *right,
                &format!("{}.divide", integer.intrinsic_name()),
            ),
            IntrinsicFunction::IntegerCompare { integer, operation } => {
                let predicate = match (operation, integer.is_signed()) {
                    (IntegerCompareOperation::Equal, _) => inkwell::IntPredicate::EQ,
                    (IntegerCompareOperation::NotEqual, _) => inkwell::IntPredicate::NE,
                    (IntegerCompareOperation::LessThan, true) => inkwell::IntPredicate::SLT,
                    (IntegerCompareOperation::LessThan, false) => inkwell::IntPredicate::ULT,
                    (IntegerCompareOperation::LessThanOrEqual, true) => inkwell::IntPredicate::SLE,
                    (IntegerCompareOperation::LessThanOrEqual, false) => inkwell::IntPredicate::ULE,
                    (IntegerCompareOperation::GreaterThan, true) => inkwell::IntPredicate::SGT,
                    (IntegerCompareOperation::GreaterThan, false) => inkwell::IntPredicate::UGT,
                    (IntegerCompareOperation::GreaterThanOrEqual, true) => {
                        inkwell::IntPredicate::SGE
                    }
                    (IntegerCompareOperation::GreaterThanOrEqual, false) => {
                        inkwell::IntPredicate::UGE
                    }
                };
                let condition = self
                    .builder
                    .build_int_compare(
                        predicate,
                        *left,
                        *right,
                        &format!("{}.compare", integer.intrinsic_name()),
                    )
                    .map_err(|error| {
                        Diagnostic::new(call.syntax.span.clone(), error.to_string())
                    })?;
                return self.compile_bool(condition, call.syntax.id, call.syntax.span.clone());
            }
            IntrinsicFunction::StringFromCString
            | IntrinsicFunction::StringToCString
            | IntrinsicFunction::StringAdd
            | IntrinsicFunction::ToString { .. }
            | IntrinsicFunction::FloatBinary { .. }
            | IntrinsicFunction::FloatCompare { .. }
            | IntrinsicFunction::SliceLength
            | IntrinsicFunction::SliceFromRef
            | IntrinsicFunction::BufferWithCapacity
            | IntrinsicFunction::BufferLength
            | IntrinsicFunction::BufferCapacity
            | IntrinsicFunction::BufferPush
            | IntrinsicFunction::BufferPop
            | IntrinsicFunction::BufferGet
            | IntrinsicFunction::BufferFreeze
            | IntrinsicFunction::BufferTransfer
            | IntrinsicFunction::BufferClone
            | IntrinsicFunction::RefReplace
            | IntrinsicFunction::Drop
            | IntrinsicFunction::ReactiveScope
            | IntrinsicFunction::Reaction
            | IntrinsicFunction::Batch
            | IntrinsicFunction::Snapshot => {
                unreachable!()
            }
        }
        .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        Ok(value.as_any_value_enum())
    }

    fn compile_reaction(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let implicit = self
            .typed_module
            .implicit_thunk_for(call.argument.syntax().id)
            .cloned();
        let callback = if let Some(thunk) = &implicit {
            self.build_closure(environment, thunk.id, call.argument.syntax().span.clone())?
                .as_any_value_enum()
        } else {
            self.compile_expression(environment, &call.argument)?
        };
        let AnyValueEnum::StructValue(callback) = callback else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "reaction callback is not a closure",
            ));
        };
        let callback_type = if let Some(thunk) = &implicit {
            self.typed_module.type_of_function(thunk.id).cloned()
        } else {
            self.concrete_expression_type(&call.argument)
                .and_then(|ty| match ty {
                    CheckedType::Function(function) => Some(function),
                    _ => None,
                })
        }
        .ok_or_else(|| {
            Diagnostic::new(
                call.argument.syntax().span.clone(),
                "reaction callback has no function type",
            )
        })?;
        let resources = self.compile_resource_arguments(
            environment,
            &callback_type.effects,
            call.syntax.span.clone(),
        )?;
        let scope = environment
            .resources
            .iter()
            .rev()
            .find_map(|bound| {
                if !self
                    .typed_module
                    .is_reactive_type(&bound.resource.value_type)
                {
                    return None;
                }
                let value = value_as_basic(bound.value)?;
                if bound.indirect {
                    self.builder
                        .build_load(
                            self.compile_type(&bound.resource.value_type).ok()?,
                            value.into_pointer_value(),
                            "reactive.resource",
                        )
                        .ok()
                        .map(|value| value.into_pointer_value())
                } else {
                    Some(value.into_pointer_value())
                }
            })
            .ok_or_else(|| {
                Diagnostic::new(
                    call.syntax.span.clone(),
                    "resource `Reactive` is not available",
                )
            })?;

        let mut payload_fields = vec![callback.get_type().into()];
        for resource in &callback_type.effects.resources {
            if resource.mutable
                || !self
                    .typed_module
                    .is_copy_in_function(&resource.value_type, environment.function_id)
            {
                payload_fields.push(self.context.ptr_type(AddressSpace::default()).into());
            } else {
                payload_fields.push(self.compile_type(&resource.value_type)?);
            }
        }
        let payload_type = self.context.struct_type(&payload_fields, false);
        let payload = self
            .builder
            .build_malloc(payload_type, "reaction.payload")
            .map_err(compiler_diagnostic)?;
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload, 0, "reaction.callback")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(callback_slot, callback)
            .map_err(compiler_diagnostic)?;
        for (index, resource) in resources.iter().enumerate() {
            let slot = self
                .builder
                .build_struct_gep(
                    payload_type,
                    payload,
                    (index + 1) as u32,
                    "reaction.resource",
                )
                .map_err(compiler_diagnostic)?;
            let resource = BasicValueEnum::try_from(*resource).map_err(|_| {
                Diagnostic::new(
                    call.syntax.span.clone(),
                    "reaction resource is not first-class",
                )
            })?;
            self.builder
                .build_store(slot, resource)
                .map_err(compiler_diagnostic)?;
        }

        let previous = self.builder.get_insert_block();
        let runner_type = self.context.void_type().fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let runner = self.llvm_module.add_function(
            &format!("__staple_reaction_runner_{}", call.syntax.id.0),
            runner_type,
            Some(inkwell::module::Linkage::Internal),
        );
        let entry = self.context.append_basic_block(runner, "entry");
        self.builder.position_at_end(entry);
        let payload_argument = runner.get_first_param().unwrap().into_pointer_value();
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload_argument, 0, "reaction.callback")
            .map_err(compiler_diagnostic)?;
        let loaded_callback = self
            .builder
            .build_load(callback.get_type(), callback_slot, "reaction.callback")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        let code = self
            .builder
            .build_extract_value(loaded_callback, 0, "reaction.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(loaded_callback, 1, "reaction.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let mut arguments = vec![closure_environment.into()];
        for (index, resource) in callback_type.effects.resources.iter().enumerate() {
            let slot = self
                .builder
                .build_struct_gep(
                    payload_type,
                    payload_argument,
                    (index + 1) as u32,
                    "reaction.resource",
                )
                .map_err(compiler_diagnostic)?;
            arguments.push(
                self.builder
                    .build_load(
                        if resource.mutable
                            || !self
                                .typed_module
                                .is_copy_in_function(&resource.value_type, environment.function_id)
                        {
                            self.context.ptr_type(AddressSpace::default()).into()
                        } else {
                            self.compile_type(&resource.value_type)?
                        },
                        slot,
                        "reaction.resource",
                    )
                    .map_err(compiler_diagnostic)?
                    .into(),
            );
        }
        self.builder
            .build_indirect_call(
                self.compile_closure_function_type(&callback_type)?,
                code,
                &arguments,
                "reaction.call",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = previous {
            self.builder.position_at_end(block);
        }
        let payload_size = self
            .size_type
            .const_int(self.target_data.get_store_size(&payload_type), false);
        self.build_reactive_runtime_call(
            "__staple_reaction_create",
            &[
                scope.into(),
                runner.as_global_value().as_pointer_value().into(),
                payload.into(),
                payload_size.into(),
            ],
            None,
            "reaction.create",
            call.syntax.span.clone(),
        )?;
        Ok(self.unit_value())
    }

    fn compile_batch(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let implicit = self
            .typed_module
            .implicit_thunk_for(call.argument.syntax().id)
            .cloned();
        let callback = if let Some(thunk) = &implicit {
            self.build_closure(environment, thunk.id, call.argument.syntax().span.clone())?
                .as_any_value_enum()
        } else {
            self.compile_expression(environment, &call.argument)?
        };
        let AnyValueEnum::StructValue(callback) = callback else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "batch callback is not a closure",
            ));
        };
        let callback_type = if let Some(thunk) = &implicit {
            self.typed_module.type_of_function(thunk.id).cloned()
        } else {
            self.concrete_expression_type(&call.argument)
                .and_then(|ty| match ty {
                    CheckedType::Function(function) => Some(function),
                    _ => None,
                })
        }
        .ok_or_else(|| {
            Diagnostic::new(
                call.argument.syntax().span.clone(),
                "batch callback has no function type",
            )
        })?;
        let resources = self.compile_resource_arguments(
            environment,
            &callback_type.effects,
            call.syntax.span.clone(),
        )?;

        self.build_reactive_runtime_call(
            "__staple_batch_begin",
            &[],
            None,
            "batch.begin",
            call.syntax.span.clone(),
        )?;
        let code = self
            .builder
            .build_extract_value(callback, 0, "batch.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(callback, 1, "batch.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let mut arguments = vec![closure_environment.into()];
        arguments.extend(resources);
        self.builder
            .build_indirect_call(
                self.compile_closure_function_type(&callback_type)?,
                code,
                &arguments,
                "batch.call",
            )
            .map_err(compiler_diagnostic)?;
        self.build_reactive_runtime_call(
            "__staple_batch_end",
            &[],
            None,
            "batch.end",
            call.syntax.span.clone(),
        )?;
        Ok(self.unit_value())
    }

    fn compile_derived_create(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        symbol: SymbolId,
        value_slot: inkwell::values::PointerValue<'context>,
        metadata_slot: inkwell::values::PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let evaluator = self
            .typed_module
            .derived_evaluator(symbol)
            .cloned()
            .ok_or_else(|| Diagnostic::new(span.clone(), "derived evaluator is unavailable"))?;
        let callback = self
            .build_closure(environment, evaluator.id, span.clone())?
            .as_any_value_enum();
        let AnyValueEnum::StructValue(callback) = callback else {
            return Err(Diagnostic::new(span, "derived evaluator is not a closure"));
        };
        let callback_type = self
            .typed_module
            .type_of_function(evaluator.id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "derived evaluator has no function type")
            })?;
        if !callback_type.effects.resources.is_empty() {
            return Err(Diagnostic::new(
                span,
                "derived evaluators cannot capture resources",
            ));
        }

        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let payload_type = self
            .context
            .struct_type(&[callback.get_type().into(), pointer_type.into()], false);
        let payload = self
            .builder
            .build_malloc(payload_type, "derived.payload")
            .map_err(compiler_diagnostic)?;
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload, 0, "derived.callback")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(callback_slot, callback)
            .map_err(compiler_diagnostic)?;
        let output_slot = self
            .builder
            .build_struct_gep(payload_type, payload, 1, "derived.output")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(output_slot, value_slot)
            .map_err(compiler_diagnostic)?;

        let previous = self.builder.get_insert_block();
        let runner_type = self
            .context
            .void_type()
            .fn_type(&[pointer_type.into()], false);
        let runner = self.llvm_module.add_function(
            &format!("__staple_derived_runner_{}", evaluator.id.0),
            runner_type,
            Some(inkwell::module::Linkage::Internal),
        );
        let entry = self.context.append_basic_block(runner, "entry");
        self.builder.position_at_end(entry);
        let payload_argument = runner.get_first_param().unwrap().into_pointer_value();
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload_argument, 0, "derived.callback")
            .map_err(compiler_diagnostic)?;
        let loaded_callback = self
            .builder
            .build_load(callback.get_type(), callback_slot, "derived.callback")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        let code = self
            .builder
            .build_extract_value(loaded_callback, 0, "derived.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(loaded_callback, 1, "derived.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let call = self
            .builder
            .build_indirect_call(
                self.compile_closure_function_type(&callback_type)?,
                code,
                &[closure_environment.into()],
                "derived.evaluate",
            )
            .map_err(compiler_diagnostic)?;
        let value = call.try_as_basic_value().basic().ok_or_else(|| {
            Diagnostic::new(span.clone(), "derived evaluator result is not storable")
        })?;
        let output_slot = self
            .builder
            .build_struct_gep(payload_type, payload_argument, 1, "derived.output")
            .map_err(compiler_diagnostic)?;
        let output = self
            .builder
            .build_load(pointer_type, output_slot, "derived.output")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        self.builder
            .build_store(output, value)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = previous {
            self.builder.position_at_end(block);
        }

        let payload_size = self
            .size_type
            .const_int(self.target_data.get_store_size(&payload_type), false);
        let derived = self
            .build_reactive_runtime_call(
                "__staple_derived_create",
                &[
                    runner.as_global_value().as_pointer_value().into(),
                    payload.into(),
                    payload_size.into(),
                ],
                Some(pointer_type.into()),
                "derived.create",
                span,
            )?
            .expect("derived_create returns a pointer");
        self.builder
            .build_store(metadata_slot, derived)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    fn compile_numeric_to_string(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
        numeric: NumericType,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let argument = self.compile_expression(environment, &call.argument)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let capacity = self.context.i32_type().const_int(128, false);
        let buffer = self
            .builder
            .build_array_alloca(self.context.i8_type(), capacity, "to_string.buffer")
            .map_err(compiler_diagnostic)?;
        let format = match numeric {
            NumericType::Integer(integer) if integer.is_signed() => "%lld",
            NumericType::Integer(_) => "%llu",
            NumericType::Float(FloatType::F32) => "%.9g",
            NumericType::Float(FloatType::F64) => "%.17g",
        };
        let format = self
            .builder
            .build_global_string_ptr(format, "to_string.format")
            .map_err(compiler_diagnostic)?
            .as_pointer_value();
        let formatted = match (numeric, value_as_basic(argument)) {
            (NumericType::Integer(integer), Some(BasicValueEnum::IntValue(value))) => {
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
                BasicValueEnum::IntValue(wide)
            }
            (NumericType::Float(FloatType::F32), Some(BasicValueEnum::FloatValue(value))) => {
                BasicValueEnum::FloatValue(
                    self.builder
                        .build_float_ext(value, self.context.f64_type(), "to_string.float")
                        .map_err(compiler_diagnostic)?,
                )
            }
            (NumericType::Float(FloatType::F64), Some(BasicValueEnum::FloatValue(value))) => {
                BasicValueEnum::FloatValue(value)
            }
            _ => {
                return Err(Diagnostic::new(
                    call.syntax.span.clone(),
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
        let snprintf = self
            .llvm_module
            .get_function("snprintf")
            .unwrap_or_else(|| {
                self.llvm_module
                    .add_function("snprintf", snprintf_type, None)
            });
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
        let pointer =
            self.build_gc_allocation(length, "to_string.data", call.syntax.span.clone())?;
        self.builder
            .build_memcpy(pointer, 1, buffer, 1, length)
            .map_err(compiler_diagnostic)?;
        Ok(self
            .build_string_value(pointer, length, call.syntax.span.clone())?
            .as_any_value_enum())
    }

    fn compile_string_add(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let arguments = self.compile_arguments(environment, &call.argument, 2, false)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let [
            inkwell::values::BasicMetadataValueEnum::StructValue(left),
            inkwell::values::BasicMetadataValueEnum::StructValue(right),
        ] = arguments.as_slice()
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "string concatenation requires two String values",
            ));
        };
        let left_pointer = self
            .builder
            .build_extract_value(*left, 0, "string.add.left.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let left_length = self
            .builder
            .build_extract_value(*left, 1, "string.add.left.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let right_pointer = self
            .builder
            .build_extract_value(*right, 0, "string.add.right.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let right_length = self
            .builder
            .build_extract_value(*right, 1, "string.add.right.length")
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
        self.build_trap_if(overflow, call.syntax.span.clone())?;
        let pointer =
            self.build_gc_allocation(length, "string.add.data", call.syntax.span.clone())?;
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
            .build_string_value(pointer, length, call.syntax.span.clone())?
            .as_any_value_enum())
    }

    fn compile_bool(
        &self,
        condition: inkwell::values::IntValue<'context>,
        syntax: crate::SyntaxId,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let CheckedType::Sum(sum) = self
            .typed_module
            .type_of_expression(syntax)
            .cloned()
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(span, "comparison result must be Bool"));
        };
        if sum.alternatives.len() != 2 {
            return Err(Diagnostic::new(span, "comparison result must be Bool"));
        }
        // `Bool` is declared as `True | False` in the standard-library contract.
        let true_index = 0;
        let false_index = 1;
        let sum_type = self.compile_sum_type(&sum)?;
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

    fn compile_string_from_c_string(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let argument = self.compile_expression(environment, &call.argument)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let Some(BasicValueEnum::PointerValue(source)) = value_as_basic(argument) else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "CString conversion requires a pointer",
            ));
        };
        let strlen_type = self.size_type.fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let strlen = self
            .llvm_module
            .get_function("strlen")
            .unwrap_or_else(|| self.llvm_module.add_function("strlen", strlen_type, None));
        let length = self
            .builder
            .build_direct_call(strlen, &[source.into()], "c_string.length")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let validator = self
            .llvm_module
            .get_function("__staple_is_valid_utf8")
            .expect("UTF-8 validator is declared before function bodies");
        let valid = self
            .builder
            .build_direct_call(
                validator,
                &[source.into(), length.into()],
                "c_string.valid_utf8",
            )
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let invalid = self
            .builder
            .build_not(valid, "c_string.invalid_utf8")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.build_trap_if(invalid, call.syntax.span.clone())?;
        let pointer = self.build_gc_allocation(length, "string.data", call.syntax.span.clone())?;
        self.builder
            .build_memcpy(pointer, 1, source, 1, length)
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        let result = self.build_string_value(pointer, length, call.syntax.span.clone())?;
        self.compile_drop_value(
            source.into(),
            &CheckedType::CString,
            call.syntax.span.clone(),
        )?;
        Ok(result.as_any_value_enum())
    }

    fn compile_string_to_c_string(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let argument = self.compile_expression(environment, &call.argument)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let Some(BasicValueEnum::StructValue(string)) = value_as_basic(argument) else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "String conversion requires a String value",
            ));
        };
        let pointer = self
            .builder
            .build_extract_value(string, 0, "string.pointer")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?
            .into_pointer_value();
        let length = self
            .builder
            .build_extract_value(string, 1, "string.length")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?
            .into_int_value();

        let memchr_type = self.context.ptr_type(AddressSpace::default()).fn_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.context.i32_type().into(),
                self.size_type.into(),
            ],
            false,
        );
        let memchr = self
            .llvm_module
            .get_function("memchr")
            .unwrap_or_else(|| self.llvm_module.add_function("memchr", memchr_type, None));
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
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let has_nul = self
            .builder
            .build_is_not_null(nul, "string.has_interior_nul")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.build_trap_if(has_nul, call.syntax.span.clone())?;

        let allocation_length = self
            .builder
            .build_int_add(
                length,
                self.size_type.const_int(1, false),
                "c_string.length",
            )
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        let overflow = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                allocation_length,
                length,
                "c_string.length_overflow",
            )
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.build_trap_if(overflow, call.syntax.span.clone())?;
        let result = self
            .builder
            .build_array_malloc(self.context.i8_type(), allocation_length, "c_string.data")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.builder
            .build_memcpy(result, 1, pointer, 1, length)
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        let terminator = unsafe {
            self.builder.build_gep(
                self.context.i8_type(),
                result,
                &[length],
                "c_string.terminator",
            )
        }
        .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.builder
            .build_store(terminator, self.context.i8_type().const_zero())
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        Ok(result.as_any_value_enum())
    }

    fn build_trap_if(
        &mut self,
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

    fn build_utf8_validator(&mut self) -> CodeGenerationResult<()> {
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

        let pointer = function.get_nth_param(0).unwrap().into_pointer_value();
        let length = function.get_nth_param(1).unwrap().into_int_value();
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
        Ok(())
    }

    fn increment_utf8_index(
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

    fn byte_in_range(
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

    fn byte_equals(
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

    fn build_closure(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        function_id: FunctionId,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let code = self.functions[&function_id];
        self.build_closure_with_code(environment, function_id, code, span)
    }

    fn build_closure_with_code(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        function_id: FunctionId,
        code: inkwell::values::FunctionValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let function = self
            .typed_module
            .function_by_id(function_id)
            .cloned()
            .ok_or_else(|| Diagnostic::new(span.clone(), "unknown function"))?;
        let environment_pointer = if function.captures.is_empty() {
            self.context.ptr_type(AddressSpace::default()).const_null()
        } else {
            let environment_type = self.compile_capture_type(&function)?;
            let mut environment_value = environment_type.const_zero();
            for (index, symbol) in function.captures.iter().copied().enumerate() {
                let value = if self
                    .typed_module
                    .resolved()
                    .requires_initialization_state(symbol)
                    || self.typed_module.has_mutable_storage(symbol)
                    || self.typed_module.is_derived_symbol(symbol)
                {
                    environment
                        .parameter_pointers
                        .get(&symbol)
                        .copied()
                        .or_else(|| environment.binding_cells.get(&symbol).copied())
                        .ok_or_else(|| {
                            Diagnostic::new(span.clone(), "captured binding cell is not available")
                        })?
                        .into()
                } else {
                    let value = self.compile_symbol_value(
                        environment,
                        symbol,
                        false,
                        span.clone(),
                        "captured value is not available here".to_owned(),
                    )?;
                    value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(span.clone(), "captured value is not first-class")
                    })?
                };
                environment_value = self
                    .builder
                    .build_insert_value(environment_value, value, index as u32, "capture")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                    .into_struct_value();
            }
            let pointer = self.build_gc_allocation(
                self.size_type
                    .const_int(self.target_data.get_store_size(&environment_type), false),
                "closure.environment",
                span.clone(),
            )?;
            self.builder
                .build_store(pointer, environment_value)
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            if function.captures.iter().copied().any(|symbol| {
                !self
                    .typed_module
                    .resolved()
                    .requires_initialization_state(symbol)
                    && self
                        .typed_module
                        .type_of_symbol(symbol)
                        .cloned()
                        .map(|ty| substitute_type(ty, &self.active_type_substitutions))
                        .is_some_and(|ty| self.typed_module.type_needs_drop(&ty))
            }) {
                let finalizer = self.ensure_closure_finalizer(&function, environment_type)?;
                self.set_gc_finalizer(pointer, finalizer)?;
            }
            pointer
        };
        self.build_closure_value(code, environment_pointer)
    }

    fn ensure_closure_finalizer(
        &mut self,
        closure: &ResolvedFunction,
        environment_type: inkwell::types::StructType<'context>,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let capture_types = closure
            .captures
            .iter()
            .filter_map(|symbol| self.typed_module.type_of_symbol(*symbol))
            .cloned()
            .map(|ty| substitute_type(ty, &self.active_type_substitutions))
            .collect::<Vec<_>>();
        let key = format!("closure:{}:{capture_types:?}", closure.id.0);
        if let Some(function) = self.gc_finalizers.get(&key).copied() {
            return Ok(function);
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let name = format!("__staple_gc_finalize_closure_{:016x}", hasher.finish());
        let function_type = self.context.void_type().fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let function = self.llvm_module.add_function(&name, function_type, None);
        self.gc_finalizers.insert(key, function);

        let previous_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);
        let pointer = function
            .get_first_param()
            .expect("closure payload")
            .into_pointer_value();
        let environment = self
            .builder
            .build_load(environment_type, pointer, "closure.finalizer.environment")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        for (index, symbol) in closure.captures.iter().copied().enumerate().rev() {
            if self
                .typed_module
                .resolved()
                .requires_initialization_state(symbol)
                || self.typed_module.has_mutable_storage(symbol)
                || self.typed_module.is_derived_symbol(symbol)
            {
                continue;
            }
            let Some(value_type) = self.typed_module.type_of_symbol(symbol).cloned() else {
                continue;
            };
            let value_type = substitute_type(value_type, &self.active_type_substitutions);
            if !self.typed_module.type_needs_drop(&value_type) {
                continue;
            }
            let value = self
                .builder
                .build_extract_value(environment, index as u32, "closure.finalizer.capture")
                .map_err(compiler_diagnostic)?;
            self.compile_drop_value(value, &value_type, Span::Compiler)?;
        }
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = previous_block {
            self.builder.position_at_end(block);
        }
        Ok(function)
    }

    fn build_closure_value(
        &mut self,
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

    fn compile_capture_type(
        &self,
        function: &ResolvedFunction,
    ) -> CodeGenerationResult<inkwell::types::StructType<'context>> {
        let fields = function
            .captures
            .iter()
            .map(|symbol| {
                if self
                    .typed_module
                    .resolved()
                    .requires_initialization_state(*symbol)
                    || self.typed_module.has_mutable_storage(*symbol)
                    || self.typed_module.is_derived_symbol(*symbol)
                {
                    return Ok(self.context.ptr_type(AddressSpace::default()).into());
                }
                self.typed_module
                    .type_of_symbol(*symbol)
                    .ok_or_else(|| Diagnostic::new(Span::Compiler, "unchecked capture"))
                    .map(|ty| substitute_type(ty.clone(), &self.active_type_substitutions))
                    .and_then(|ty| self.compile_type(&ty))
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        Ok(self.context.struct_type(&fields, false))
    }

    fn compile_product_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        product: &ProductExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let values = self.compile_product_elements(environment, product)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        if let [value] = values.as_slice() {
            return Ok(value.as_any_value_enum());
        }
        let types = values
            .iter()
            .map(BasicValueEnum::get_type)
            .collect::<Vec<_>>();
        let mut product_value = self.context.struct_type(&types, true).const_zero();
        for (index, element) in values.into_iter().enumerate() {
            product_value = self
                .builder
                .build_insert_value(product_value, element, index as u32, "product.element")
                .map_err(|error| Diagnostic::new(product.syntax.span.clone(), error.to_string()))?
                .into_struct_value();
        }
        Ok(product_value.as_any_value_enum())
    }

    /// Compiles `(value; count)`: evaluate `value` once and copy it into every
    /// slot of the fixed-arity product. Type checking has already normalized the
    /// node's type to a concrete product (or, for `count == 1`, the bare element
    /// type), so the arity is read straight from that.
    fn compile_repeated_product_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        repeated: &RepeatedProductExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let literal_count = || match repeated.count.as_ref() {
            Expression::Integer(integer) => integer.literal.parse::<usize>().unwrap_or(1),
            _ => 1,
        };
        let count = match self
            .typed_module
            .type_of_expression(repeated.syntax.id)
            .cloned()
            .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))
        {
            Some(CheckedType::Product(product)) if !product.variadic => product.elements.len(),
            _ => literal_count(),
        };

        let value = self.compile_adapted_call_argument(environment, &repeated.value)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        if !matches!(repeated.count.as_ref(), Expression::Integer(_)) {
            let _ = self.compile_expression(environment, &repeated.count)?;
            if environment.did_return {
                return Ok(self.unit_value());
            }
        }
        if count == 1 {
            return Ok(value);
        }
        let Some(value) = value_as_basic(value) else {
            return Err(Diagnostic::new(
                repeated.value.syntax().span.clone(),
                "repeated product element has an invalid representation",
            ));
        };
        let elements = vec![value; count];
        Ok(self
            .build_product_value(&elements, repeated.syntax.span.clone())?
            .as_any_value_enum())
    }

    fn compile_product_elements(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        product: &ProductExpression,
    ) -> CodeGenerationResult<Vec<BasicValueEnum<'context>>> {
        if product.elements.iter().any(|element| element.designated) {
            return self.compile_designated_product_elements(environment, product);
        }
        if product.elements.iter().any(|element| element.named_spread) {
            return self.compile_named_spread_product_elements(environment, product);
        }
        let mut values = Vec::new();
        for element in &product.elements {
            let value = self.compile_adapted_call_argument(environment, &element.value)?;
            if environment.did_return {
                return Ok(Vec::new());
            }
            if element.spread {
                let Some(CheckedType::Product(value_type)) =
                    self.concrete_expression_type(&element.value)
                else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand does not have a fixed product type",
                    ));
                };
                if value_type.elements.is_empty() {
                    continue;
                }
                let Some(BasicValueEnum::StructValue(product_value)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand has an invalid representation",
                    ));
                };
                for index in 0..value_type.elements.len() {
                    values.push(
                        self.builder
                            .build_extract_value(
                                product_value,
                                index as u32,
                                "product.spread.element",
                            )
                            .map_err(|error| {
                                Diagnostic::new(element.syntax.span.clone(), error.to_string())
                            })?,
                    );
                }
                continue;
            }
            values.push(value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    element.syntax.span.clone(),
                    "product element is not a first-class value",
                )
            })?);
        }
        if let Some(plan) = self
            .typed_module
            .product_default_plan(product.syntax.id)
            .cloned()
        {
            let mut positioned = vec![None; plan.defaults.len()];
            for (index, value) in values.into_iter().enumerate() {
                positioned[index] = Some(value);
            }
            self.compile_product_default_plan(environment, &plan, positioned)
        } else {
            Ok(values)
        }
    }

    fn compile_product_default_plan(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        plan: &crate::typecheck::CheckedProductDefaultPlan,
        mut values: Vec<Option<BasicValueEnum<'context>>>,
    ) -> CodeGenerationResult<Vec<BasicValueEnum<'context>>> {
        for (index, default) in plan.defaults.iter().enumerate() {
            if values[index].is_none()
                && let Some(default) = default
            {
                let previous = self.expression_type_overrides.insert(
                    default.syntax().id,
                    plan.final_type.elements[index].value_type.clone(),
                );
                let compiled = self.compile_expression(environment, default);
                if let Some(previous) = previous {
                    self.expression_type_overrides
                        .insert(default.syntax().id, previous);
                } else {
                    self.expression_type_overrides.remove(&default.syntax().id);
                }
                let value = compiled?;
                values[index] = Some(value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        default.syntax().span.clone(),
                        "product field default is not first-class",
                    )
                })?);
            }
        }
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.ok_or_else(|| {
                    Diagnostic::new(
                        Span::Compiler,
                        format!("missing product element at position {index}"),
                    )
                })
            })
            .collect()
    }

    /// Evaluates a designated product in source order, but stores each value
    /// at the position selected by the expression's checked expected type.
    fn compile_designated_product_elements(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        product: &ProductExpression,
    ) -> CodeGenerationResult<Vec<BasicValueEnum<'context>>> {
        let Some(CheckedType::Product(final_type)) =
            self.concrete_expression_type(&Expression::Product(product.clone()))
        else {
            return Err(Diagnostic::new(
                product.syntax.span.clone(),
                "designated product initializer does not have a known product type",
            ));
        };
        let mut values = vec![None; final_type.elements.len()];
        let mut positional_index = 0usize;
        for element in &product.elements {
            let value = self.compile_adapted_call_argument(environment, &element.value)?;
            if environment.did_return {
                return Ok(Vec::new());
            }
            if element.designated {
                let name = element
                    .name
                    .as_deref()
                    .expect("designators always have a name");
                let index = final_type
                    .elements
                    .iter()
                    .position(|field| field.name.as_deref() == Some(name))
                    .ok_or_else(|| {
                        Diagnostic::new(
                            element.syntax.span.clone(),
                            format!("unknown designated product field `{name}`"),
                        )
                    })?;
                values[index] = Some(value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        element.syntax.span.clone(),
                        "product element is not a first-class value",
                    )
                })?);
                continue;
            }
            if element.spread {
                let Some(CheckedType::Product(value_type)) =
                    self.concrete_expression_type(&element.value)
                else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand does not have a fixed product type",
                    ));
                };
                if value_type.elements.is_empty() {
                    continue;
                }
                let Some(BasicValueEnum::StructValue(product_value)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand has an invalid representation",
                    ));
                };
                for index in 0..value_type.elements.len() {
                    values[positional_index] = Some(
                        self.builder
                            .build_extract_value(
                                product_value,
                                index as u32,
                                "product.spread.element",
                            )
                            .map_err(|error| {
                                Diagnostic::new(element.syntax.span.clone(), error.to_string())
                            })?,
                    );
                    positional_index += 1;
                }
                continue;
            }
            values[positional_index] = Some(value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    element.syntax.span.clone(),
                    "product element is not a first-class value",
                )
            })?);
            positional_index += 1;
        }
        if let Some(plan) = self
            .typed_module
            .product_default_plan(product.syntax.id)
            .cloned()
        {
            self.compile_product_default_plan(environment, &plan, values)
        } else {
            values
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    value.ok_or_else(|| {
                        Diagnostic::new(
                            product.syntax.span.clone(),
                            format!("missing product element at position {index}"),
                        )
                    })
                })
                .collect()
        }
    }

    /// Compiles a product literal that contains one or more `...=` named
    /// spreads. Every contributing element (explicit or spread) is first
    /// gathered into a name-keyed map, then reassembled in the order of the
    /// expression's already-checked final type so the resulting struct
    /// layout matches what typechecking computed.
    fn compile_named_spread_product_elements(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        product: &ProductExpression,
    ) -> CodeGenerationResult<Vec<BasicValueEnum<'context>>> {
        let mut fields: HashMap<String, BasicValueEnum<'context>> = HashMap::new();
        for element in &product.elements {
            let value = self.compile_adapted_call_argument(environment, &element.value)?;
            if environment.did_return {
                return Ok(Vec::new());
            }
            if element.spread {
                let Some(CheckedType::Product(value_type)) =
                    self.concrete_expression_type(&element.value)
                else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand does not have a fixed product type",
                    ));
                };
                if value_type.elements.is_empty() {
                    continue;
                }
                let Some(BasicValueEnum::StructValue(product_value)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand has an invalid representation",
                    ));
                };
                for (index, field_type) in value_type.elements.iter().enumerate() {
                    let name = field_type.name.clone().ok_or_else(|| {
                        Diagnostic::new(
                            element.syntax.span.clone(),
                            "a named spread operand must have every element named",
                        )
                    })?;
                    let extracted = self
                        .builder
                        .build_extract_value(product_value, index as u32, "product.spread.element")
                        .map_err(|error| {
                            Diagnostic::new(element.syntax.span.clone(), error.to_string())
                        })?;
                    fields.insert(name, extracted);
                }
                continue;
            }
            let name = element.name.clone().ok_or_else(|| {
                Diagnostic::new(
                    element.syntax.span.clone(),
                    "every element must be named when the product contains a named spread",
                )
            })?;
            let basic = value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    element.syntax.span.clone(),
                    "product element is not a first-class value",
                )
            })?;
            fields.insert(name, basic);
        }
        let Some(CheckedType::Product(final_type)) =
            self.concrete_expression_type(&Expression::Product(product.clone()))
        else {
            return Err(Diagnostic::new(
                product.syntax.span.clone(),
                "named product spread does not have a known product type",
            ));
        };
        final_type
            .elements
            .iter()
            .map(|field| {
                let name = field
                    .name
                    .clone()
                    .expect("named spread result fields are always named");
                fields.get(&name).copied().ok_or_else(|| {
                    Diagnostic::new(
                        product.syntax.span.clone(),
                        format!("missing field `{name}` in named product spread"),
                    )
                })
            })
            .collect()
    }

    fn compile_effect_arguments(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        argument: &Expression,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<CompiledCallArguments<'context>> {
        let mutations = &function_type.mutations;
        if mutations.is_empty() {
            let expected = self
                .compile_parameter_types(&function_type.parameter)?
                .len();
            return Ok(CompiledCallArguments {
                values: self.compile_arguments(environment, argument, expected, false)?,
                temporaries: Vec::new(),
            });
        }
        if mutations.contains(&CheckedMutation::Whole) {
            let (pointer, temporary) = self.compile_mutation_argument_pointer(
                environment,
                argument,
                &function_type.parameter,
            )?;
            return Ok(CompiledCallArguments {
                values: vec![pointer.into()],
                temporaries: temporary.into_iter().collect(),
            });
        }

        let types = flattened_parameter_types(&function_type.parameter);
        let mask = mutation_parameter_mask(types.len(), mutations);
        if let Some(plan) = self
            .typed_module
            .product_default_plan(argument.syntax().id)
            .cloned()
        {
            let mut values = vec![None; plan.defaults.len()];
            let mut temporaries = Vec::new();
            let mut positional = 0usize;
            let explicit = match argument {
                Expression::Product(product)
                    if product.elements.iter().all(|element| !element.spread) =>
                {
                    product
                        .elements
                        .iter()
                        .map(|element| {
                            let index = if element.designated {
                                plan.final_type
                                    .elements
                                    .iter()
                                    .position(|field| field.name == element.name)
                                    .expect("checked designated field")
                            } else {
                                let index = positional;
                                positional += 1;
                                index
                            };
                            (index, &element.value)
                        })
                        .collect::<Vec<_>>()
                }
                Expression::Product(_) => Vec::new(),
                _ => vec![(0, argument)],
            };
            for (index, expression) in explicit {
                if mask[index] {
                    let (pointer, temporary) = self.compile_mutation_argument_pointer(
                        environment,
                        expression,
                        types[index],
                    )?;
                    values[index] = Some(pointer.into());
                    temporaries.extend(temporary);
                } else {
                    let value = self.compile_expression(environment, expression)?;
                    values[index] = Some(
                        value_as_basic(value)
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    expression.syntax().span.clone(),
                                    "argument is not first-class",
                                )
                            })?
                            .into(),
                    );
                }
            }
            for (index, default) in plan.defaults.iter().enumerate() {
                if values[index].is_some() {
                    continue;
                }
                let default = default.as_ref().expect("checked default field");
                let previous = self.expression_type_overrides.insert(
                    default.syntax().id,
                    plan.final_type.elements[index].value_type.clone(),
                );
                if mask[index] {
                    let compiled =
                        self.compile_mutation_argument_pointer(environment, default, types[index]);
                    if let Some(previous) = previous {
                        self.expression_type_overrides
                            .insert(default.syntax().id, previous);
                    } else {
                        self.expression_type_overrides.remove(&default.syntax().id);
                    }
                    let (pointer, temporary) = compiled?;
                    values[index] = Some(pointer.into());
                    temporaries.extend(temporary);
                } else {
                    let compiled = self.compile_expression(environment, default);
                    if let Some(previous) = previous {
                        self.expression_type_overrides
                            .insert(default.syntax().id, previous);
                    } else {
                        self.expression_type_overrides.remove(&default.syntax().id);
                    }
                    let value = compiled?;
                    values[index] = Some(
                        value_as_basic(value)
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    default.syntax().span.clone(),
                                    "product field default is not first-class",
                                )
                            })?
                            .into(),
                    );
                }
            }
            return Ok(CompiledCallArguments {
                values: values.into_iter().map(Option::unwrap).collect(),
                temporaries,
            });
        }
        let expressions = match argument {
            Expression::Product(product) if product.elements.len() == types.len() => Some(
                product
                    .elements
                    .iter()
                    .map(|element| &element.value)
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        if let Some(expressions) = expressions {
            let mut values = Vec::new();
            let mut temporaries = Vec::new();
            for (index, expression) in expressions.into_iter().enumerate() {
                if mask[index] {
                    let (pointer, temporary) = self.compile_mutation_argument_pointer(
                        environment,
                        expression,
                        types[index],
                    )?;
                    values.push(pointer.into());
                    temporaries.extend(temporary);
                } else {
                    let value = self.compile_expression(environment, expression)?;
                    values.push(
                        value_as_basic(value)
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    expression.syntax().span.clone(),
                                    "argument is not first-class",
                                )
                            })?
                            .into(),
                    );
                }
            }
            return Ok(CompiledCallArguments {
                values,
                temporaries,
            });
        }

        // A product-valued place passed without literal destructuring: take
        // addresses of mutated fields and load the remaining fields.
        if let CheckedType::Product(product) = &*function_type.parameter
            && let Ok((base, _, _)) = self.compile_place_pointer(environment, argument)
        {
            let llvm_product = self
                .compile_type(&function_type.parameter)?
                .into_struct_type();
            let mut values = Vec::new();
            for (index, element) in product.elements.iter().enumerate() {
                let field = self
                    .builder
                    .build_struct_gep(llvm_product, base, index as u32, "argument.field")
                    .map_err(compiler_diagnostic)?;
                if mask[index] {
                    values.push(field.into());
                } else {
                    values.push(
                        self.builder
                            .build_load(
                                self.compile_type(&element.value_type)?,
                                field,
                                "argument.value",
                            )
                            .map_err(compiler_diagnostic)?
                            .into(),
                    );
                }
            }
            return Ok(CompiledCallArguments {
                values,
                temporaries: Vec::new(),
            });
        }

        Err(Diagnostic::new(
            argument.syntax().span.clone(),
            "mutation-affected product argument cannot be addressed",
        ))
    }

    fn compile_mutation_argument_pointer(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        expression: &Expression,
        value_type: &CheckedType,
    ) -> CodeGenerationResult<(
        inkwell::values::PointerValue<'context>,
        Option<(inkwell::values::PointerValue<'context>, CheckedType)>,
    )> {
        if expression_has_place_root(self.typed_module.resolved(), expression) {
            let (pointer, _, _) = self.compile_place_pointer(environment, expression)?;
            return Ok((pointer, None));
        }
        let value = self.compile_expression(environment, expression)?;
        let value = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(expression.syntax().span.clone(), "argument is not storable")
        })?;
        let concrete = substitute_type(value_type.clone(), &self.active_type_substitutions);
        let pointer = self
            .builder
            .build_alloca(self.compile_type(&concrete)?, "mutation.temporary")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(pointer, value)
            .map_err(compiler_diagnostic)?;
        Ok((pointer, Some((pointer, concrete))))
    }

    fn drop_mutation_temporaries(
        &mut self,
        temporaries: Vec<(inkwell::values::PointerValue<'context>, CheckedType)>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        for (pointer, value_type) in temporaries.into_iter().rev() {
            if self.typed_module.type_needs_drop(&value_type) {
                let value = self
                    .builder
                    .build_load(
                        self.compile_type(&value_type)?,
                        pointer,
                        "mutation.temporary.final",
                    )
                    .map_err(compiler_diagnostic)?;
                self.compile_drop_value(value, &value_type, span.clone())?;
            }
        }
        Ok(())
    }

    fn compile_arguments(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        argument: &Expression,
        expected_count: usize,
        variadic: bool,
    ) -> CodeGenerationResult<Vec<inkwell::values::BasicMetadataValueEnum<'context>>> {
        let mut arguments =
            if let Some(thunk) = self.typed_module.implicit_thunk_for(argument.syntax().id) {
                vec![
                    self.build_closure(environment, thunk.id, argument.syntax().span.clone())?
                        .as_basic_value_enum(),
                ]
            } else if let Expression::Product(product) = argument {
                self.compile_product_elements(environment, product)?
            } else {
                let value = self.compile_expression(environment, argument)?;
                if environment.did_return {
                    Vec::new()
                } else {
                    vec![value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(
                            argument.syntax().span.clone(),
                            "argument is not a first-class value",
                        )
                    })?]
                }
            };
        if let Some(plan) = self
            .typed_module
            .product_default_plan(argument.syntax().id)
            .cloned()
        {
            let mut positioned = vec![None; plan.defaults.len()];
            for (index, value) in arguments.into_iter().enumerate() {
                positioned[index] = Some(value);
            }
            arguments = self.compile_product_default_plan(environment, &plan, positioned)?;
        }
        if environment.did_return {
            return Ok(Vec::new());
        }
        if arguments.len() == expected_count || (variadic && arguments.len() >= expected_count) {
            return Ok(arguments.into_iter().map(Into::into).collect());
        }
        if let [BasicValueEnum::StructValue(product)] = arguments.as_slice()
            && product.get_type().count_fields() as usize == expected_count
        {
            return (0..expected_count)
                .map(|index| {
                    self.builder
                        .build_extract_value(*product, index as u32, "argument.element")
                        .map(Into::into)
                        .map_err(|error| {
                            Diagnostic::new(argument.syntax().span.clone(), error.to_string())
                        })
                })
                .collect();
        }
        Err(Diagnostic::new(
            argument.syntax().span.clone(),
            "argument layout does not match the called function",
        ))
    }

    fn compile_adapted_call_argument(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        expression: &Expression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if let Some(thunk) = self.typed_module.implicit_thunk_for(expression.syntax().id) {
            return self
                .build_closure(environment, thunk.id, expression.syntax().span.clone())
                .map(Into::into);
        }
        self.compile_expression(environment, expression)
    }

    fn build_product_value(
        &mut self,
        values: &[BasicValueEnum<'context>],
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        if let [value] = values {
            return Ok(*value);
        }
        let types = values
            .iter()
            .map(BasicValueEnum::get_type)
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

    fn compile_native_function_type(
        &self,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<inkwell::types::FunctionType<'context>> {
        let return_type = self.compile_type(&function_type.result)?;
        let parameter_types = self.compile_parameter_types(&function_type.parameter)?;
        let variadic = matches!(
            &*function_type.parameter,
            CheckedType::Product(product) if product.variadic
        );
        Ok(match return_type {
            inkwell::types::BasicTypeEnum::ArrayType(value) => {
                value.fn_type(&parameter_types, variadic)
            }
            inkwell::types::BasicTypeEnum::FloatType(value) => {
                value.fn_type(&parameter_types, variadic)
            }
            inkwell::types::BasicTypeEnum::IntType(value) => {
                value.fn_type(&parameter_types, variadic)
            }
            inkwell::types::BasicTypeEnum::PointerType(value) => {
                value.fn_type(&parameter_types, variadic)
            }
            inkwell::types::BasicTypeEnum::StructType(value) => {
                value.fn_type(&parameter_types, variadic)
            }
            inkwell::types::BasicTypeEnum::VectorType(_)
            | inkwell::types::BasicTypeEnum::ScalableVectorType(_) => {
                return Err(Diagnostic::new(
                    Span::Compiler,
                    "vector return types are not supported",
                ));
            }
        })
    }

    fn compile_closure_function_type(
        &self,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<inkwell::types::FunctionType<'context>> {
        let return_type = self.compile_type(&function_type.result)?;
        let mut parameter_types = vec![self.context.ptr_type(AddressSpace::default()).into()];
        for resource in &function_type.effects.resources {
            if resource.mutable
                || !self
                    .typed_module
                    .is_copy_in_function(&resource.value_type, None)
            {
                parameter_types.push(self.context.ptr_type(AddressSpace::default()).into());
            } else {
                parameter_types.push(self.compile_type(&resource.value_type)?.into());
            }
        }
        let value_parameters = if function_type.mutations.contains(&CheckedMutation::Whole) {
            vec![self.context.ptr_type(AddressSpace::default()).into()]
        } else {
            let mut parameters = self.compile_parameter_types(&function_type.parameter)?;
            let mutation_mask = mutation_parameter_mask(parameters.len(), &function_type.mutations);
            for (index, parameter) in parameters.iter_mut().enumerate() {
                if mutation_mask[index] {
                    *parameter = self.context.ptr_type(AddressSpace::default()).into();
                }
            }
            parameters
        };
        parameter_types.extend(value_parameters);
        Ok(match return_type {
            inkwell::types::BasicTypeEnum::ArrayType(value) => {
                value.fn_type(&parameter_types, false)
            }
            inkwell::types::BasicTypeEnum::FloatType(value) => {
                value.fn_type(&parameter_types, false)
            }
            inkwell::types::BasicTypeEnum::IntType(value) => value.fn_type(&parameter_types, false),
            inkwell::types::BasicTypeEnum::PointerType(value) => {
                value.fn_type(&parameter_types, false)
            }
            inkwell::types::BasicTypeEnum::StructType(value) => {
                value.fn_type(&parameter_types, false)
            }
            inkwell::types::BasicTypeEnum::VectorType(_)
            | inkwell::types::BasicTypeEnum::ScalableVectorType(_) => {
                return Err(Diagnostic::new(
                    Span::Compiler,
                    "vector return types are not supported",
                ));
            }
        })
    }

    fn compile_parameter_types(
        &self,
        parameter_type: &CheckedType,
    ) -> CodeGenerationResult<Vec<inkwell::types::BasicMetadataTypeEnum<'context>>> {
        match parameter_type {
            CheckedType::Product(product) => product
                .elements
                .iter()
                .map(|element| self.compile_type(&element.value_type).map(Into::into))
                .collect(),
            other => Ok(vec![self.compile_type(other)?.into()]),
        }
    }

    fn compile_type(
        &self,
        value_type: &CheckedType,
    ) -> CodeGenerationResult<inkwell::types::BasicTypeEnum<'context>> {
        match value_type {
            CheckedType::Inferred => Err(Diagnostic::new(
                Span::Compiler,
                "cannot generate code for an inferred type before type checking",
            )),
            CheckedType::Error => Err(Diagnostic::new(
                Span::Compiler,
                "cannot generate code for an erroneous type",
            )),
            CheckedType::NumberLiteral(_) => {
                Ok(self.compile_integer_type(IntegerType::USize).into())
            }
            CheckedType::RepeatedProduct { .. } => Err(Diagnostic::new(
                Span::Compiler,
                "cannot generate code for a homogeneous product with an unspecialized size",
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
            CheckedType::Opaque { .. } if self.typed_module.is_io_type(value_type) => {
                Ok(self.context.struct_type(&[], false).into())
            }
            CheckedType::Opaque { .. } if self.typed_module.is_reactive_type(value_type) => {
                Ok(self.context.ptr_type(AddressSpace::default()).into())
            }
            CheckedType::Opaque { name, .. } => Err(Diagnostic::new(
                Span::Compiler,
                format!("opaque type `{name}` has no by-value representation"),
            )),
            CheckedType::CString => Ok(self.context.ptr_type(AddressSpace::default()).into()),
            CheckedType::String | CheckedType::StringLiteralSet(_) => self
                .typed_module
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
            CheckedType::ErasedProduct(_) => Err(Diagnostic::new(
                Span::Compiler,
                "an erased product cannot be represented by value",
            )),
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
            CheckedType::Distinct { representation, .. } => self.compile_type(representation),
        }
    }

    fn compile_integer_type(&self, integer: IntegerType) -> inkwell::types::IntType<'context> {
        match integer {
            IntegerType::I8 | IntegerType::U8 => self.context.i8_type(),
            IntegerType::I16 | IntegerType::U16 => self.context.i16_type(),
            IntegerType::I32 | IntegerType::U32 => self.context.i32_type(),
            IntegerType::I64 | IntegerType::U64 => self.context.i64_type(),
            IntegerType::ISize | IntegerType::USize => self.size_type,
        }
    }

    fn compile_float_type(&self, float: FloatType) -> inkwell::types::FloatType<'context> {
        match float {
            FloatType::F32 => self.context.f32_type(),
            FloatType::F64 => self.context.f64_type(),
        }
    }

    fn closure_type(&self) -> inkwell::types::StructType<'context> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        self.context
            .struct_type(&[pointer.into(), pointer.into()], false)
    }

    fn compile_product_type(
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

    fn compile_sum_type(
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
}

fn create_target_machine(target: Option<&str>) -> CodeGenerationResult<TargetMachine> {
    Target::initialize_all(&InitializationConfig::default());
    let triple = target
        .map(TargetTriple::create)
        .unwrap_or_else(TargetMachine::get_default_triple);
    let target = Target::from_triple(&triple)
        .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
    target
        .create_target_machine(
            &triple,
            "generic",
            "",
            OptimizationLevel::Default,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| Diagnostic::new(Span::Compiler, "could not create LLVM target machine"))
}

/// Whether a resolved function `candidate` denotes the standard-library
/// function whose source name is `name`. Standard-library definitions are
/// emitted with their bare source name in single-module builds but are
/// mangled to `__staple_m{module}_{name}` once the program spans more than
/// one non-standard module (see the name mangling in `resolve`), so a bare
/// string comparison misses them in package builds.
fn standard_function_name_matches(candidate: &str, name: &str) -> bool {
    if candidate == name {
        return true;
    }
    let Some(rest) = candidate.strip_prefix("__staple_m") else {
        return false;
    };
    let digits = rest.find(|character: char| !character.is_ascii_digit());
    match digits {
        Some(offset) if offset > 0 => rest[offset..].strip_prefix('_') == Some(name),
        _ => false,
    }
}

fn value_as_basic(value: AnyValueEnum<'_>) -> Option<BasicValueEnum<'_>> {
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

fn checked_type_contains_ref(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::Ref(_) | CheckedType::Buffer(_) => true,
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| checked_type_contains_ref(&element.value_type)),
        CheckedType::Sum(sum) => sum.alternatives.iter().any(checked_type_contains_ref),
        CheckedType::Function(function) => {
            checked_type_contains_ref(&function.parameter)
                || checked_type_contains_ref(&function.result)
        }
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(checked_type_contains_ref)
                || checked_type_contains_ref(representation)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(checked_type_contains_ref)
        }
        CheckedType::CPointer { pointee } => checked_type_contains_ref(pointee),
        _ => false,
    }
}

fn strip_place_wrappers(mut value_type: CheckedType) -> CheckedType {
    loop {
        match value_type {
            CheckedType::Distinct { representation, .. } => value_type = *representation,
            other => return other,
        }
    }
}

impl<'module, 'context> ModuleEmitter<'module, 'context> {
    fn buffer_header_type(
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

    fn buffer_data_pointer(
        &self,
        buffer: inkwell::values::PointerValue<'context>,
        element: BasicTypeEnum<'context>,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        self.builder
            .build_struct_gep(self.buffer_header_type(element), buffer, 3, "buffer.data")
            .map_err(compiler_diagnostic)
    }

    fn compile_buffer_with_capacity(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let capacity = self.compile_expression(environment, &call.argument)?;
        let Some(BasicValueEnum::IntValue(capacity)) = value_as_basic(capacity) else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "Buffer.with_capacity requires a USize capacity",
            ));
        };
        let CheckedType::Buffer(element) = self
            .concrete_expression_type(&Expression::Call(call.clone()))
            .ok_or_else(|| Diagnostic::new(call.syntax.span.clone(), "unchecked Buffer result"))?
        else {
            return Err(Diagnostic::new(
                call.syntax.span.clone(),
                "invalid Buffer result type",
            ));
        };
        let llvm_element = self.compile_type(&element)?;
        let header = self.buffer_header_type(llvm_element);
        let offset = self
            .target_data
            .offset_of_element(&header, 3)
            .expect("Buffer data field has an offset");
        let stride = self.target_data.get_abi_size(&llvm_element);
        let maximum = if self.size_type.get_bit_width() == 64 {
            u64::MAX
        } else {
            u32::MAX as u64
        };
        if stride != 0 {
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
            self.build_trap_if(too_large, call.syntax.span.clone())?;
        }
        let bytes = self
            .builder
            .build_int_mul(
                capacity,
                self.size_type.const_int(stride, false),
                "buffer.element.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let no_element_bytes = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                bytes,
                self.size_type.const_zero(),
                "buffer.no.element.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let bytes = self
            .builder
            .build_select(
                no_element_bytes,
                self.size_type.const_int(1, false),
                bytes,
                "buffer.physical.element.bytes",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let bytes = self
            .builder
            .build_int_add(
                bytes,
                self.size_type.const_int(offset, false),
                "buffer.allocation.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let buffer =
            self.build_gc_allocation(bytes, "buffer.allocate", call.syntax.span.clone())?;
        self.builder
            .build_memset(
                buffer,
                self.target_data.get_abi_alignment(&header),
                self.context.i8_type().const_zero(),
                bytes,
            )
            .map_err(compiler_diagnostic)?;
        let capacity_slot = self
            .builder
            .build_struct_gep(header, buffer, 1, "buffer.capacity.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(capacity_slot, capacity)
            .map_err(compiler_diagnostic)?;
        if self.typed_module.type_needs_drop(&element) {
            let finalizer = self.ensure_buffer_finalizer(&element)?;
            self.set_gc_finalizer(buffer, finalizer)?;
        }
        Ok(buffer.as_any_value_enum())
    }

    fn compile_buffer_metadata(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
        field: u32,
        name: &str,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let value = self.compile_expression(environment, &call.argument)?;
        let Some(BasicValueEnum::PointerValue(buffer)) = value_as_basic(value) else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer handle",
            ));
        };
        let CheckedType::Buffer(element) = self
            .concrete_expression_type(&call.argument)
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer type",
            ));
        };
        let header = self.buffer_header_type(self.compile_type(&element)?);
        let slot = self
            .builder
            .build_struct_gep(header, buffer, field, name)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_load(self.size_type, slot, name)
            .map(|value| value.as_any_value_enum())
            .map_err(compiler_diagnostic)
    }

    fn compile_buffer_push(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let arguments = self.compile_arguments(environment, &call.argument, 2, false)?;
        let [
            inkwell::values::BasicMetadataValueEnum::PointerValue(buffer),
            replacement,
        ] = arguments.as_slice()
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "Buffer.push requires a buffer and value",
            ));
        };
        let CheckedType::Product(product) = self
            .concrete_expression_type(&call.argument)
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer.push arguments",
            ));
        };
        let CheckedType::Buffer(element) = &product.elements[0].value_type else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer.push target",
            ));
        };
        let llvm_element = self.compile_type(element)?;
        let header = self.buffer_header_type(llvm_element);
        self.trap_if_buffer_frozen(*buffer, header, call.syntax.span.clone())?;
        let length_slot = self
            .builder
            .build_struct_gep(header, *buffer, 0, "buffer.length.slot")
            .map_err(compiler_diagnostic)?;
        let capacity_slot = self
            .builder
            .build_struct_gep(header, *buffer, 1, "buffer.capacity.slot")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_load(self.size_type, length_slot, "buffer.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let capacity = self
            .builder
            .build_load(self.size_type, capacity_slot, "buffer.capacity")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let full = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGE, length, capacity, "buffer.full")
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(full, call.syntax.span.clone())?;
        let data = self.buffer_data_pointer(*buffer, llvm_element)?;
        let slot = unsafe {
            self.builder
                .build_gep(llvm_element, data, &[length], "buffer.push.slot")
        }
        .map_err(compiler_diagnostic)?;
        let replacement = BasicValueEnum::try_from(*replacement).map_err(|_| {
            Diagnostic::new(call.syntax.span.clone(), "Buffer element is not storable")
        })?;
        self.builder
            .build_store(slot, replacement)
            .map_err(compiler_diagnostic)?;
        let next = self
            .builder
            .build_int_add(
                length,
                self.size_type.const_int(1, false),
                "buffer.next.length",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(length_slot, next)
            .map_err(compiler_diagnostic)?;
        Ok(self.unit_value())
    }

    fn compile_buffer_get(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let arguments = self.compile_arguments(environment, &call.argument, 2, false)?;
        let [
            inkwell::values::BasicMetadataValueEnum::PointerValue(buffer),
            inkwell::values::BasicMetadataValueEnum::IntValue(position),
        ] = arguments.as_slice()
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "Buffer.get_ref requires a buffer and USize index",
            ));
        };
        let CheckedType::Ref(element) = self
            .concrete_expression_type(&Expression::Call(call.clone()))
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.syntax.span.clone(),
                "invalid Buffer.get_ref result",
            ));
        };
        let llvm_element = self.compile_type(&element)?;
        let header = self.buffer_header_type(llvm_element);
        let length_slot = self
            .builder
            .build_struct_gep(header, *buffer, 0, "buffer.length.slot")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_load(self.size_type, length_slot, "buffer.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let out = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGE,
                *position,
                length,
                "buffer.get.out_of_bounds",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(out, call.syntax.span.clone())?;
        let data = self.buffer_data_pointer(*buffer, llvm_element)?;
        let reference = unsafe {
            self.builder
                .build_gep(llvm_element, data, &[*position], "buffer.get.reference")
        }
        .map_err(compiler_diagnostic)?;
        self.register_gc_interior(reference, *buffer)?;
        Ok(reference.as_any_value_enum())
    }

    fn compile_buffer_pop(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let value = self.compile_expression(environment, &call.argument)?;
        let Some(BasicValueEnum::PointerValue(buffer)) = value_as_basic(value) else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer handle",
            ));
        };
        let CheckedType::Buffer(element) = self
            .concrete_expression_type(&call.argument)
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer.pop target",
            ));
        };
        let CheckedType::Sum(option) = self
            .concrete_expression_type(&Expression::Call(call.clone()))
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.syntax.span.clone(),
                "Buffer.pop must return Option T",
            ));
        };
        let none_index = option.alternatives.iter().position(|alternative| {
            matches!(alternative, CheckedType::Distinct { name, .. } if name.ends_with("None"))
        }).ok_or_else(|| Diagnostic::new(call.syntax.span.clone(), "Option is missing None"))?;
        let some_index = option.alternatives.iter().position(|alternative| {
            matches!(alternative, CheckedType::Distinct { name, .. } if name.ends_with("Some"))
        }).ok_or_else(|| Diagnostic::new(call.syntax.span.clone(), "Option is missing Some"))?;
        let llvm_element = self.compile_type(&element)?;
        let header = self.buffer_header_type(llvm_element);
        self.trap_if_buffer_frozen(buffer, header, call.syntax.span.clone())?;
        let length_slot = self
            .builder
            .build_struct_gep(header, buffer, 0, "buffer.length.slot")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_load(self.size_type, length_slot, "buffer.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let empty = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                length,
                self.size_type.const_zero(),
                "buffer.empty",
            )
            .map_err(compiler_diagnostic)?;
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .expect("Buffer.pop is in a function");
        let none_block = self.context.append_basic_block(function, "buffer.pop.none");
        let some_block = self.context.append_basic_block(function, "buffer.pop.some");
        let merge = self.context.append_basic_block(function, "buffer.pop.done");
        let option_type = self.compile_sum_type(&option)?;
        let result_slot = self
            .builder
            .build_alloca(option_type, "buffer.pop.result")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(result_slot, option_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let tag_slot = self
            .builder
            .build_struct_gep(option_type, result_slot, 0, "buffer.pop.tag")
            .map_err(compiler_diagnostic)?;
        let payload_slot = self
            .builder
            .build_struct_gep(option_type, result_slot, 1, "buffer.pop.payload")
            .map_err(compiler_diagnostic)?;
        let payload_type = option_type
            .get_field_type_at_index(1)
            .expect("Option payload");
        let storage = SumStorage {
            tag: tag_slot,
            payload: payload_slot,
            alignment: self.target_data.get_abi_alignment(&payload_type),
        };
        self.builder
            .build_conditional_branch(empty, none_block, some_block)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(none_block);
        self.builder
            .build_store(
                tag_slot,
                self.context.i32_type().const_int(none_index as u64, false),
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(merge)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(some_block);
        let next = self
            .builder
            .build_int_sub(
                length,
                self.size_type.const_int(1, false),
                "buffer.pop.index",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(length_slot, next)
            .map_err(compiler_diagnostic)?;
        let data = self.buffer_data_pointer(buffer, llvm_element)?;
        let slot = unsafe {
            self.builder
                .build_gep(llvm_element, data, &[next], "buffer.pop.slot")
        }
        .map_err(compiler_diagnostic)?;
        let popped = self
            .builder
            .build_load(llvm_element, slot, "buffer.pop.value")
            .map_err(compiler_diagnostic)?;
        self.store_sum_payload(
            popped.as_any_value_enum(),
            &option.alternatives[some_index],
            some_index,
            &storage,
            call.syntax.span.clone(),
        )?;
        self.builder
            .build_memset(
                slot,
                self.target_data.get_abi_alignment(&llvm_element),
                self.context.i8_type().const_zero(),
                self.size_type
                    .const_int(self.target_data.get_store_size(&llvm_element), false),
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(merge)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(merge);
        self.builder
            .build_load(option_type, result_slot, "buffer.pop.option")
            .map(|value| value.as_any_value_enum())
            .map_err(compiler_diagnostic)
    }

    fn compile_buffer_freeze(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let value = self.compile_expression(environment, &call.argument)?;
        let Some(BasicValueEnum::PointerValue(buffer)) = value_as_basic(value) else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer handle",
            ));
        };
        let CheckedType::Buffer(element) = self
            .concrete_expression_type(&call.argument)
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer type",
            ));
        };
        let llvm_element = self.compile_type(&element)?;
        let header = self.buffer_header_type(llvm_element);
        let frozen_slot = self
            .builder
            .build_struct_gep(header, buffer, 2, "buffer.frozen.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(frozen_slot, self.context.i8_type().const_int(1, false))
            .map_err(compiler_diagnostic)?;
        let length_slot = self
            .builder
            .build_struct_gep(header, buffer, 0, "buffer.length.slot")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_load(self.size_type, length_slot, "buffer.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let data = self.buffer_data_pointer(buffer, llvm_element)?;
        self.register_gc_interior(data, buffer)?;
        let mut result = self.slice_type().const_zero();
        result = self
            .builder
            .build_insert_value(result, data, 0, "buffer.slice.pointer")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        result = self
            .builder
            .build_insert_value(result, length, 1, "buffer.slice.length")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        Ok(result.as_any_value_enum())
    }

    fn compile_buffer_transfer(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let arguments = self.compile_arguments(environment, &call.argument, 2, false)?;
        let [
            inkwell::values::BasicMetadataValueEnum::PointerValue(source),
            inkwell::values::BasicMetadataValueEnum::PointerValue(destination),
        ] = arguments.as_slice()
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "Buffer.transfer requires two buffers",
            ));
        };
        let CheckedType::Product(product) = self
            .concrete_expression_type(&call.argument)
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer.transfer arguments",
            ));
        };
        let CheckedType::Buffer(element) = &product.elements[0].value_type else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer.transfer source",
            ));
        };
        let llvm_element = self.compile_type(element)?;
        let header = self.buffer_header_type(llvm_element);

        let aliased = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                *source,
                *destination,
                "buffer.transfer.aliased",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(aliased, call.syntax.span.clone())?;

        self.trap_if_buffer_frozen(*source, header, call.syntax.span.clone())?;
        self.trap_if_buffer_frozen(*destination, header, call.syntax.span.clone())?;

        let source_length_slot = self
            .builder
            .build_struct_gep(header, *source, 0, "buffer.transfer.source.length.slot")
            .map_err(compiler_diagnostic)?;
        let source_length = self
            .builder
            .build_load(
                self.size_type,
                source_length_slot,
                "buffer.transfer.source.length",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();

        let dest_length_slot = self
            .builder
            .build_struct_gep(header, *destination, 0, "buffer.transfer.dest.length.slot")
            .map_err(compiler_diagnostic)?;
        let dest_length = self
            .builder
            .build_load(
                self.size_type,
                dest_length_slot,
                "buffer.transfer.dest.length",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let dest_capacity_slot = self
            .builder
            .build_struct_gep(
                header,
                *destination,
                1,
                "buffer.transfer.dest.capacity.slot",
            )
            .map_err(compiler_diagnostic)?;
        let dest_capacity = self
            .builder
            .build_load(
                self.size_type,
                dest_capacity_slot,
                "buffer.transfer.dest.capacity",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();

        let dest_remaining = self
            .builder
            .build_int_sub(dest_capacity, dest_length, "buffer.transfer.dest.remaining")
            .map_err(compiler_diagnostic)?;
        let insufficient = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                dest_remaining,
                source_length,
                "buffer.transfer.insufficient_capacity",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(insufficient, call.syntax.span.clone())?;

        let source_data = self.buffer_data_pointer(*source, llvm_element)?;
        let dest_data = self.buffer_data_pointer(*destination, llvm_element)?;
        let dest_write = unsafe {
            self.builder.build_gep(
                llvm_element,
                dest_data,
                &[dest_length],
                "buffer.transfer.dest.write",
            )
        }
        .map_err(compiler_diagnostic)?;

        let stride = self.target_data.get_abi_size(&llvm_element);
        let bytes = self
            .builder
            .build_int_mul(
                source_length,
                self.size_type.const_int(stride, false),
                "buffer.transfer.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let alignment = self.target_data.get_abi_alignment(&llvm_element);
        self.builder
            .build_memcpy(dest_write, alignment, source_data, alignment, bytes)
            .map_err(compiler_diagnostic)?;

        let new_dest_length = self
            .builder
            .build_int_add(
                dest_length,
                source_length,
                "buffer.transfer.dest.next_length",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(dest_length_slot, new_dest_length)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(source_length_slot, self.size_type.const_zero())
            .map_err(compiler_diagnostic)?;

        Ok(self.unit_value())
    }

    fn compile_buffer_clone(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let value = self.compile_expression(environment, &call.argument)?;
        let Some(BasicValueEnum::PointerValue(source)) = value_as_basic(value) else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer handle",
            ));
        };
        let CheckedType::Buffer(element) = self
            .concrete_expression_type(&call.argument)
            .unwrap_or(CheckedType::Error)
        else {
            return Err(Diagnostic::new(
                call.argument.syntax().span.clone(),
                "invalid Buffer type",
            ));
        };
        let llvm_element = self.compile_type(&element)?;
        let header = self.buffer_header_type(llvm_element);
        let source_length_slot = self
            .builder
            .build_struct_gep(header, source, 0, "buffer.clone.source.length.slot")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_load(self.size_type, source_length_slot, "buffer.clone.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let source_capacity_slot = self
            .builder
            .build_struct_gep(header, source, 1, "buffer.clone.source.capacity.slot")
            .map_err(compiler_diagnostic)?;
        let capacity = self
            .builder
            .build_load(
                self.size_type,
                source_capacity_slot,
                "buffer.clone.capacity",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();

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
                "buffer.clone.element.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let no_element_bytes = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                bytes,
                self.size_type.const_zero(),
                "buffer.clone.no.element.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let bytes = self
            .builder
            .build_select(
                no_element_bytes,
                self.size_type.const_int(1, false),
                bytes,
                "buffer.clone.physical.element.bytes",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let bytes = self
            .builder
            .build_int_add(
                bytes,
                self.size_type.const_int(offset, false),
                "buffer.clone.allocation.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let destination =
            self.build_gc_allocation(bytes, "buffer.clone.allocate", call.syntax.span.clone())?;
        self.builder
            .build_memset(
                destination,
                self.target_data.get_abi_alignment(&header),
                self.context.i8_type().const_zero(),
                bytes,
            )
            .map_err(compiler_diagnostic)?;
        let destination_capacity_slot = self
            .builder
            .build_struct_gep(
                header,
                destination,
                1,
                "buffer.clone.destination.capacity.slot",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(destination_capacity_slot, capacity)
            .map_err(compiler_diagnostic)?;
        if self.typed_module.type_needs_drop(&element) {
            let finalizer = self.ensure_buffer_finalizer(&element)?;
            self.set_gc_finalizer(destination, finalizer)?;
        }

        let clone_trait = self
            .typed_module
            .resolved()
            .standard_trait("Clone")
            .ok_or_else(|| {
                Diagnostic::new(Span::Compiler, "standard library has no Clone trait")
            })?;
        let clone_method = self
            .typed_module
            .resolved()
            .traits()
            .get(&clone_trait)
            .and_then(|trait_| trait_.methods.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "Clone trait has no clone method"))?;
        let clone_function = self.trait_method_code(
            clone_trait,
            std::slice::from_ref(element.as_ref()),
            clone_method,
            call.syntax.span.clone(),
        )?;
        let source_data = self.buffer_data_pointer(source, llvm_element)?;
        let destination_data = self.buffer_data_pointer(destination, llvm_element)?;
        let destination_length_slot = self
            .builder
            .build_struct_gep(
                header,
                destination,
                0,
                "buffer.clone.destination.length.slot",
            )
            .map_err(compiler_diagnostic)?;
        let index_slot = self
            .builder
            .build_alloca(self.size_type, "buffer.clone.index.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(index_slot, self.size_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .expect("buffer clone function");
        let condition = self
            .context
            .append_basic_block(function, "buffer.clone.condition");
        let body = self
            .context
            .append_basic_block(function, "buffer.clone.body");
        let done = self
            .context
            .append_basic_block(function, "buffer.clone.done");
        self.builder
            .build_unconditional_branch(condition)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(condition);
        let index = self
            .builder
            .build_load(self.size_type, index_slot, "buffer.clone.index")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let has_element = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                index,
                length,
                "buffer.clone.has.element",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(has_element, body, done)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(body);
        let source_slot = unsafe {
            self.builder.build_gep(
                llvm_element,
                source_data,
                &[index],
                "buffer.clone.source.slot",
            )
        }
        .map_err(compiler_diagnostic)?;
        let source_element = self
            .builder
            .build_load(llvm_element, source_slot, "buffer.clone.source.element")
            .map_err(compiler_diagnostic)?;
        let closure_environment = self.context.ptr_type(AddressSpace::default()).const_null();
        let cloned = self
            .builder
            .build_direct_call(
                clone_function,
                &[closure_environment.into(), source_element.into()],
                "buffer.clone.element",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| {
                Diagnostic::new(call.syntax.span.clone(), "Clone result is not first-class")
            })?;
        let destination_slot = unsafe {
            self.builder.build_gep(
                llvm_element,
                destination_data,
                &[index],
                "buffer.clone.destination.slot",
            )
        }
        .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(destination_slot, cloned)
            .map_err(compiler_diagnostic)?;
        let next = self
            .builder
            .build_int_add(
                index,
                self.size_type.const_int(1, false),
                "buffer.clone.next",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(destination_length_slot, next)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(index_slot, next)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(condition)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(done);
        Ok(destination.as_any_value_enum())
    }

    fn trap_if_buffer_frozen(
        &mut self,
        buffer: inkwell::values::PointerValue<'context>,
        header: inkwell::types::StructType<'context>,
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

    fn register_gc_interior(
        &mut self,
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

    fn ensure_buffer_finalizer(
        &mut self,
        element: &CheckedType,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let key = format!("buffer:{element:?}");
        if let Some(function) = self.gc_finalizers.get(&key).copied() {
            return Ok(function);
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let name = format!("__staple_gc_finalize_buffer_{:016x}", hasher.finish());
        let function_type = self.context.void_type().fn_type(
            &[self.context.ptr_type(AddressSpace::default()).into()],
            false,
        );
        let function = self.llvm_module.add_function(&name, function_type, None);
        self.gc_finalizers.insert(key, function);
        let previous_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(function, "entry");
        let check = self
            .context
            .append_basic_block(function, "buffer.finalize.check");
        let body = self
            .context
            .append_basic_block(function, "buffer.finalize.element");
        let done = self
            .context
            .append_basic_block(function, "buffer.finalize.done");
        self.builder.position_at_end(entry);
        let buffer = function
            .get_first_param()
            .expect("Buffer finalizer payload")
            .into_pointer_value();
        let llvm_element = self.compile_type(element)?;
        let header = self.buffer_header_type(llvm_element);
        let length_slot = self
            .builder
            .build_struct_gep(header, buffer, 0, "buffer.length.slot")
            .map_err(compiler_diagnostic)?;
        let length = self
            .builder
            .build_load(self.size_type, length_slot, "buffer.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let index_slot = self
            .builder
            .build_alloca(self.size_type, "buffer.finalize.index")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(index_slot, self.size_type.const_zero())
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(check)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(check);
        let index = self
            .builder
            .build_load(self.size_type, index_slot, "buffer.finalize.index")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let remaining = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                index,
                length,
                "buffer.finalize.remaining",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(remaining, body, done)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(body);
        let data = self.buffer_data_pointer(buffer, llvm_element)?;
        let slot = unsafe {
            self.builder
                .build_gep(llvm_element, data, &[index], "buffer.finalize.slot")
        }
        .map_err(compiler_diagnostic)?;
        let value = self
            .builder
            .build_load(llvm_element, slot, "buffer.finalize.value")
            .map_err(compiler_diagnostic)?;
        self.compile_drop_value(value, element, Span::Compiler)?;
        let next = self
            .builder
            .build_int_add(
                index,
                self.size_type.const_int(1, false),
                "buffer.finalize.next",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(index_slot, next)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(check)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(done);
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = previous_block {
            self.builder.position_at_end(block);
        }
        Ok(function)
    }
}

fn compiler_diagnostic(error: inkwell::builder::BuilderError) -> Diagnostic {
    Diagnostic::new(Span::Compiler, error.to_string())
}

fn mutation_parameter_mask(count: usize, mutations: &[CheckedMutation]) -> Vec<bool> {
    let whole = mutations.contains(&CheckedMutation::Whole);
    (0..count)
        .map(|index| whole || mutations.contains(&CheckedMutation::Element(index)))
        .collect()
}

fn flattened_parameter_types(parameter: &CheckedType) -> Vec<&CheckedType> {
    match parameter {
        CheckedType::Product(product) => product
            .elements
            .iter()
            .map(|element| &element.value_type)
            .collect(),
        other => vec![other],
    }
}

fn top_level_pattern_symbols(module: &ResolvedModule, pattern: &Pattern) -> Vec<Option<SymbolId>> {
    fn symbol(module: &ResolvedModule, pattern: &Pattern) -> Option<SymbolId> {
        match pattern {
            Pattern::Binding(binding) => module.symbol_for(binding.syntax.id),
            Pattern::At(at) => module.symbol_for(at.binding.syntax.id),
            _ => None,
        }
    }
    match pattern {
        Pattern::Product(product) => product
            .elements
            .iter()
            .map(|element| symbol(module, element))
            .collect(),
        other => vec![symbol(module, other)],
    }
}

fn expression_has_place_root(module: &ResolvedModule, expression: &Expression) -> bool {
    if module.symbol_for(expression.syntax().id).is_some() {
        return true;
    }
    match expression {
        Expression::Access(access) => expression_has_place_root(module, &access.value),
        _ => false,
    }
}
