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
    contains_type_parameter, erased_ref_length, infer_type_parameters, select_sum_alternative,
    substitute_type,
};
use crate::{
    CallExpression, CheckedFunctionType, CheckedProductType, CheckedType, Diagnostic, Expression,
    FloatType, FunctionId, IntegerBinaryOperation, IntegerCompareOperation, IntegerType,
    IntrinsicFunction, Item, ModuleId, Pattern, PatternBindingKind, ProductExpression,
    ResolvedFunction, Span, Statement, SymbolId, TypeParameterId, TypedModule,
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
    specialization_queue: Vec<(
        FunctionId,
        CheckedFunctionType,
        HashMap<TypeParameterId, CheckedType>,
    )>,
    active_type_substitutions: HashMap<TypeParameterId, CheckedType>,
    function_symbols: HashMap<SymbolId, FunctionId>,
    globals: HashMap<SymbolId, inkwell::values::AnyValueEnum<'context>>,
    closure_codes: HashMap<SymbolId, inkwell::values::FunctionValue<'context>>,
    gc_finalizers: HashMap<String, inkwell::values::FunctionValue<'context>>,
    captured_mutable_symbols: HashSet<SymbolId>,
    external_symbols: HashSet<SymbolId>,
    storage: HashMap<SymbolId, inkwell::values::GlobalValue<'context>>,
    initialization_states: HashMap<SymbolId, inkwell::values::GlobalValue<'context>>,
    initializers: HashMap<ModuleId, inkwell::values::FunctionValue<'context>>,
    size_type: inkwell::types::IntType<'context>,
    target_data: TargetData,
}

#[derive(Clone, Copy)]
struct SumStorage<'context> {
    tag: inkwell::values::PointerValue<'context>,
    payload: inkwell::values::PointerValue<'context>,
    alignment: u32,
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
    owned_mutable: HashSet<SymbolId>,
    binding_cells: HashMap<SymbolId, inkwell::values::PointerValue<'context>>,
    function_id: Option<FunctionId>,
    closure_environment: Option<inkwell::values::PointerValue<'context>>,
    did_return: bool,
    loops: Vec<LoopCodegenContext<'context>>,
}

impl<'context> FunctionEnvironment<'context> {
    fn restore_local_state(&mut self, snapshot: &Self) {
        self.locals = snapshot.locals.clone();
        self.owned = snapshot.owned.clone();
        self.owned_order = snapshot.owned_order.clone();
        self.owned_mutable = snapshot.owned_mutable.clone();
        self.binding_cells = snapshot.binding_cells.clone();
    }
}

#[derive(Clone)]
struct LoopCodegenContext<'context> {
    header: inkwell::basic_block::BasicBlock<'context>,
    exit: inkwell::basic_block::BasicBlock<'context>,
    owned_before: usize,
    incoming: Vec<(
        inkwell::values::BasicValueEnum<'context>,
        inkwell::basic_block::BasicBlock<'context>,
    )>,
}

type CodeGenerationResult<T> = Result<T, Diagnostic>;

impl<'module, 'context> ModuleEmitter<'module, 'context> {
    fn new(
        context: &'context inkwell::context::Context,
        typed_module: &'module TypedModule,
        target_machine: &TargetMachine,
    ) -> Self {
        let captured_mutable_symbols = typed_module
            .functions()
            .iter()
            .flat_map(|function| function.captures.iter().copied())
            .filter(|symbol| typed_module.resolved().is_mutable_symbol(*symbol))
            .collect();
        Self {
            context,
            typed_module,
            llvm_module: context.create_module("staple"),
            builder: context.create_builder(),
            functions: HashMap::new(),
            specialized_functions: HashMap::new(),
            constructor_codes: HashMap::new(),
            specialization_queue: Vec::new(),
            active_type_substitutions: HashMap::new(),
            function_symbols: HashMap::new(),
            globals: HashMap::new(),
            closure_codes: HashMap::new(),
            gc_finalizers: HashMap::new(),
            captured_mutable_symbols,
            external_symbols: HashSet::new(),
            storage: HashMap::new(),
            initialization_states: HashMap::new(),
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
        self.declare_external_functions()?;
        self.declare_functions()?;
        self.declare_top_level_storage()?;
        self.declare_initializers();
        self.build_utf8_validator()?;
        let typed_module = self.typed_module;
        for function in typed_module.functions() {
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
                let Item::Statement(statement) = item else {
                    continue;
                };
                if let Statement::PatternBinding(binding) = statement.as_ref() {
                    self.declare_pattern_storage(source_module.id, &binding.pattern)?;
                    continue;
                }
                let Statement::Binding(binding) = statement.as_ref() else {
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
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
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
        for function in self.typed_module.functions() {
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
                "generic function use is not fully specialized",
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
        self.typed_module
            .type_of_expression(expression.syntax().id)
            .cloned()
            .map(|value_type| substitute_type(value_type, &self.active_type_substitutions))
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
                    || self.typed_module.resolved().is_mutable_symbol(symbol)
                {
                    environment
                        .binding_cells
                        .insert(symbol, value.into_pointer_value());
                } else {
                    environment.locals.insert(symbol, value.as_any_value_enum());
                }
            }
        }
        self.bind_top_level_pattern(environment, &function.pattern, &parameters[1..])
    }

    fn bind_top_level_pattern(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        pattern: &Pattern,
        values: &[BasicValueEnum<'context>],
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Binding(_) | Pattern::Wildcard(_) | Pattern::Nominal(_) => {
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
                if binding.mutable && !self.storage.contains_key(&symbol) {
                    let cell = self.allocate_mutable_cell(
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
            if self.typed_module.resolved().is_mutable_symbol(symbol) {
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
            } else if environment.owned_mutable.remove(&symbol)
                && let Some(cell) = environment.binding_cells.get(&symbol).copied()
                && let Some(value_type) = self.typed_module.type_of_symbol(symbol).cloned()
            {
                let value_type = substitute_type(value_type, &self.active_type_substitutions);
                self.compile_conditional_mutable_cell_drop(cell, &value_type, span.clone())?;
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
            } else if environment.owned_mutable.contains(&symbol)
                && let Some(cell) = environment.binding_cells.get(&symbol).copied()
                && let Some(value_type) = self.typed_module.type_of_symbol(symbol).cloned()
            {
                let value_type = substitute_type(value_type, &self.active_type_substitutions);
                self.compile_conditional_mutable_cell_drop(cell, &value_type, span.clone())?;
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
        for (module_id, items) in modules {
            let function = self.initializers[&module_id];
            let entry = self.context.append_basic_block(function, "entry");
            self.builder.position_at_end(entry);
            let mut environment = FunctionEnvironment::default();
            for item in &items {
                if let Item::Statement(statement) = item {
                    self.compile_top_level_statement(&mut environment, statement)?;
                }
            }
            self.builder
                .build_return(None)
                .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        }
        Ok(())
    }

    fn compile_top_level_statement(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        statement: &Statement,
    ) -> CodeGenerationResult<()> {
        match statement {
            Statement::Binding(binding) => {
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
                let value = self.compile_expression(environment, expression)?;
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
            Statement::PatternBinding(binding) => {
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
            Statement::Assignment(assignment) => self.compile_assignment(environment, assignment),
            Statement::Return(statement) => Err(Diagnostic::new(
                statement.syntax.span.clone(),
                "`return` is only allowed inside a function",
            )),
            Statement::Break(statement) => Err(Diagnostic::new(
                statement.syntax.span.clone(),
                "`break` is only allowed inside a loop",
            )),
            Statement::Continue(statement) => Err(Diagnostic::new(
                statement.syntax.span.clone(),
                "`continue` is only allowed inside a loop",
            )),
            Statement::Expression(expression) => {
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
        }
    }

    fn store_pattern_globals(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        pattern: &Pattern,
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
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
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => Ok(()),
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
        statements: &[Statement],
    ) -> CodeGenerationResult<()> {
        for statement in statements {
            let Statement::Binding(binding) = statement else {
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
        Ok(self
            .context
            .struct_type(&[llvm_type, self.context.i8_type().into()], false))
    }

    fn allocate_mutable_cell(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::PointerValue<'context>> {
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            return Ok(cell);
        }
        let cell_type = self.compile_binding_cell_type(symbol)?;
        let captured = self.captured_mutable_symbols.contains(&symbol);
        let cell = if captured {
            self.build_gc_allocation(
                self.size_type
                    .const_int(self.target_data.get_store_size(&cell_type), false),
                "mutable.binding.cell",
                span.clone(),
            )?
        } else {
            self.builder
                .build_alloca(cell_type, "mutable.binding.cell")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        };
        let state = self
            .builder
            .build_struct_gep(cell_type, cell, 1, "binding.state")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.builder
            .build_store(state, self.context.i8_type().const_zero())
            .map_err(|error| Diagnostic::new(span, error.to_string()))?;
        let value_type = self
            .typed_module
            .type_of_symbol(symbol)
            .cloned()
            .map(|ty| substitute_type(ty, &self.active_type_substitutions))
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "unchecked mutable binding"))?;
        if self.typed_module.type_needs_drop(&value_type) {
            if captured {
                let finalizer = self.ensure_mutable_cell_finalizer(&value_type)?;
                self.set_gc_finalizer(cell, finalizer)?;
            } else {
                environment.owned_mutable.insert(symbol);
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
            .is_some_and(contains_type_parameter);
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

    fn compile_statement(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        statement: &Statement,
    ) -> CodeGenerationResult<Option<AnyValueEnum<'context>>> {
        match statement {
            Statement::Binding(binding) => {
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
                if binding.mutable {
                    self.allocate_mutable_cell(environment, symbol, binding.syntax.span.clone())?;
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
            Statement::PatternBinding(binding) => {
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
            Statement::Assignment(assignment) => {
                self.compile_assignment(environment, assignment)?;
                Ok(None)
            }
            Statement::Return(statement) => {
                let value = self.compile_expression(environment, &statement.value)?;
                if environment.did_return {
                    return Ok(None);
                }
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        statement.value.syntax().span.clone(),
                        "function result is not a first-class value",
                    )
                })?;
                self.drop_all_owned(environment, statement.syntax.span.clone())?;
                self.builder
                    .build_return(Some(&value))
                    .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
                environment.did_return = true;
                Ok(None)
            }
            Statement::Break(statement) => {
                let value = if let Some(expression) = &statement.value {
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
                let (exit, owned_before) = environment
                    .loops
                    .last()
                    .map(|loop_| (loop_.exit, loop_.owned_before))
                    .expect("break inside loop");
                self.drop_owned_since(environment, owned_before, statement.syntax.span.clone())?;
                self.builder
                    .build_unconditional_branch(exit)
                    .map_err(|error| {
                        Diagnostic::new(statement.syntax.span.clone(), error.to_string())
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
            Statement::Continue(statement) => {
                let (header, owned_before) = environment
                    .loops
                    .last()
                    .map(|loop_| (loop_.header, loop_.owned_before))
                    .expect("continue inside loop");
                self.drop_owned_since(environment, owned_before, statement.syntax.span.clone())?;
                self.builder
                    .build_unconditional_branch(header)
                    .map_err(|error| {
                        Diagnostic::new(statement.syntax.span.clone(), error.to_string())
                    })?;
                environment.did_return = true;
                Ok(None)
            }
            Statement::Expression(expression) => {
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
        }
    }

    fn compile_assignment(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        assignment: &crate::Assignment,
    ) -> CodeGenerationResult<()> {
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
                self.compile_conditional_mutable_cell_drop(
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
        }
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
                "mutable binding storage is not available",
            ));
        }

        match expression {
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
                if checked.erased {
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
                    let position = self.size_type.const_int(checked.index as u64, false);
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

                let (pointer, container_type) = if let Some(payload) = checked.dereference.clone() {
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
                        && self.typed_module.resolved().is_mutable_symbol(symbol)
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
                    .build_struct_gep(container_llvm, pointer, checked.index as u32, "place.field")
                    .map_err(compiler_diagnostic)?;
                Ok((pointer, result_type, None))
            }
            Expression::Index(index) => {
                let checked = self
                    .typed_module
                    .index_for(index.syntax.id)
                    .cloned()
                    .ok_or_else(|| {
                        Diagnostic::new(index.syntax.span.clone(), "missing checked index")
                    })?;
                let position = self.compile_expression(environment, &index.index)?;
                let Some(BasicValueEnum::IntValue(position)) = value_as_basic(position) else {
                    return Err(Diagnostic::new(
                        index.index.syntax().span.clone(),
                        "place index is not an integer",
                    ));
                };
                let (base, length) = match checked.kind {
                    crate::CheckedIndexKind::Value { length } => {
                        if let Some(symbol) = self.typed_module.symbol_for(index.value.syntax().id)
                            && self.typed_module.resolved().is_mutable_symbol(symbol)
                        {
                            self.check_symbol_initialization(
                                environment,
                                symbol,
                                index.value.syntax().span.clone(),
                            )?;
                        }
                        let (pointer, _, _) =
                            self.compile_place_pointer(environment, &index.value)?;
                        (pointer, self.size_type.const_int(length as u64, false))
                    }
                    crate::CheckedIndexKind::Ref { length } => {
                        let reference = self.compile_expression(environment, &index.value)?;
                        let Some(BasicValueEnum::PointerValue(pointer)) = value_as_basic(reference)
                        else {
                            return Err(Diagnostic::new(
                                index.syntax.span.clone(),
                                "invalid Ref place",
                            ));
                        };
                        (pointer, self.size_type.const_int(length as u64, false))
                    }
                    crate::CheckedIndexKind::ErasedRef => {
                        let reference = self.compile_expression(environment, &index.value)?;
                        let Some(BasicValueEnum::StructValue(reference)) =
                            value_as_basic(reference)
                        else {
                            return Err(Diagnostic::new(
                                index.syntax.span.clone(),
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
                        (pointer, length)
                    }
                };
                let out = self
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::UGE,
                        position,
                        length,
                        "place.out_of_bounds",
                    )
                    .map_err(compiler_diagnostic)?;
                self.build_trap_if(out, index.syntax.span.clone())?;
                let element_type =
                    substitute_type(checked.element.clone(), &self.active_type_substitutions);
                let llvm_type = self.compile_type(&element_type)?;
                let pointer = unsafe {
                    self.builder
                        .build_gep(llvm_type, base, &[position], "place.element")
                }
                .map_err(compiler_diagnostic)?;
                Ok((pointer, element_type, None))
            }
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
        let Pattern::Nominal(pattern) = &binding.pattern else {
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
        if let Some(dispatch) = self
            .typed_module
            .trait_dispatch_for(expression.syntax().id)
            .cloned()
        {
            let arguments = dispatch
                .arguments
                .into_iter()
                .map(|argument| substitute_type(argument, &self.active_type_substitutions))
                .collect::<Vec<_>>();
            if arguments.iter().any(contains_type_parameter) {
                return Err(Diagnostic::new(
                    expression.syntax().span.clone(),
                    "trait method arguments are not fully specialized",
                ));
            }
            let trait_id = self
                .typed_module
                .resolved()
                .trait_for_method(dispatch.method)
                .expect("trait method owner");
            let function_id = self
                .typed_module
                .trait_impl_method(trait_id, &arguments, dispatch.method)
                .ok_or_else(|| {
                    Diagnostic::new(
                        expression.syntax().span.clone(),
                        "no trait implementation is available for these arguments",
                    )
                })?;
            let function = self.trait_method_code(
                trait_id,
                &arguments,
                dispatch.method,
                expression.syntax().span.clone(),
            )?;
            return self
                .build_closure_with_code(
                    environment,
                    function_id,
                    function,
                    expression.syntax().span.clone(),
                )
                .map(|closure| closure.as_any_value_enum());
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
            Expression::Loop(loop_) => self.compile_loop_expression(environment, loop_),
            Expression::Block(block) => {
                let owned_before = environment.owned_order.len();
                self.predeclare_checked_bindings(environment, &block.statements)?;
                let mut value = None;
                for statement in &block.statements {
                    value = self.compile_statement(environment, statement)?;
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
                    if self.typed_module.resolved().builtin_type(type_id)
                        == Some(crate::BuiltinType::Ref)
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
                            || self.typed_module.resolved().is_mutable_symbol(symbol),
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
                if checked.erased {
                    let BasicValueEnum::StructValue(reference) = value else {
                        return Err(Diagnostic::new(
                            access.value.syntax().span.clone(),
                            "erased product reference has an invalid representation",
                        ));
                    };
                    let pointer = self
                        .builder
                        .build_extract_value(reference, 0, "erased_ref.pointer")
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value();
                    let length = self
                        .builder
                        .build_extract_value(reference, 1, "erased_ref.length")
                        .map_err(compiler_diagnostic)?
                        .into_int_value();
                    let position = self.size_type.const_int(checked.index as u64, false);
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
                let value = if let Some(payload) = &checked.dereference {
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
                    .build_extract_value(value, checked.index as u32, "element")
                    .map(|value| value.as_any_value_enum())
                    .map_err(|error| Diagnostic::new(access.syntax.span.clone(), error.to_string()))
            }
            Expression::Index(index) => self.compile_index_expression(environment, index),
            Expression::Infix(infix) => {
                let lowered = self
                    .typed_module
                    .resolved()
                    .lowered_infix(infix.syntax.id)
                    .cloned()
                    .ok_or_else(|| {
                        Diagnostic::new(infix.syntax.span.clone(), "unresolved infix expression")
                    })?;
                self.compile_expression(environment, &lowered)
            }
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
                        || self.typed_module.resolved().is_mutable_symbol(symbol),
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
            Expression::CString(string) => self.compile_c_string_literal(string),
            Expression::Quote(quote) => Err(Diagnostic::new(
                quote.syntax.span.clone(),
                "unexpanded `quote` expression",
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
        }
    }

    fn trait_method_code(
        &mut self,
        trait_id: crate::TraitId,
        arguments: &[CheckedType],
        method: crate::TraitMethodId,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let function_id = self
            .typed_module
            .trait_impl_method(trait_id, arguments, method)
            .ok_or_else(|| Diagnostic::new(span.clone(), "no trait implementation is available"))?;
        if let Some(function) = self.functions.get(&function_id).copied() {
            return Ok(function);
        }
        let function_type = self
            .typed_module
            .instantiated_trait_method_type(trait_id, arguments, method)
            .ok_or_else(|| Diagnostic::new(span, "trait method has no concrete function type"))?;
        self.ensure_function_specialization(function_id, &function_type)
    }

    fn compile_default_value(
        &mut self,
        value_type: &CheckedType,
        default_trait: crate::TraitId,
        method: crate::TraitMethodId,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if let CheckedType::Product(product) = value_type {
            if product.elements.is_empty() {
                return Ok(self.unit_value());
            }
            let mut values = Vec::with_capacity(product.elements.len());
            for element in &product.elements {
                let value = self.compile_default_value(
                    &element.value_type,
                    default_trait,
                    method,
                    span.clone(),
                )?;
                values.push(value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "default product element is not first-class")
                })?);
            }
            if let [value] = values.as_slice() {
                return Ok(value.as_any_value_enum());
            }
            let fields = values
                .iter()
                .map(BasicValueEnum::get_type)
                .collect::<Vec<_>>();
            let mut result = self.context.struct_type(&fields, true).const_zero();
            for (index, value) in values.into_iter().enumerate() {
                result = self
                    .builder
                    .build_insert_value(result, value, index as u32, "default.element")
                    .map_err(compiler_diagnostic)?
                    .into_struct_value();
            }
            return Ok(result.as_any_value_enum());
        }

        let function = self.trait_method_code(
            default_trait,
            std::slice::from_ref(value_type),
            method,
            span.clone(),
        )?;
        let environment = self.context.ptr_type(AddressSpace::default()).const_null();
        self.builder
            .build_direct_call(function, &[environment.into()], "default.call")
            .map_err(|error| Diagnostic::new(span, error.to_string()))?
            .try_as_basic_value()
            .basic()
            .map(AnyValueEnum::from)
            .ok_or_else(|| Diagnostic::new(Span::Compiler, "default value is not first-class"))
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
            )
        {
            Ok(value)
        } else if matches!(
            (source, target),
            (CheckedType::Ref(_), CheckedType::Ref(target))
                if matches!(target.as_ref(), CheckedType::ErasedProduct(_))
        ) {
            self.coerce_erased_ref_value(value, source, target, span)
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
        let mut value = self.erased_ref_type().const_zero();
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

    fn ensure_mutable_cell_finalizer(
        &mut self,
        value_type: &CheckedType,
    ) -> CodeGenerationResult<inkwell::values::FunctionValue<'context>> {
        let key = format!("mutable-cell:{value_type:?}");
        if let Some(function) = self.gc_finalizers.get(&key).copied() {
            return Ok(function);
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let name = format!("__staple_gc_finalize_mutable_cell_{:016x}", hasher.finish());
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
            .expect("mutable cell payload")
            .into_pointer_value();
        self.compile_conditional_mutable_cell_drop(cell, value_type, Span::Compiler)?;
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        if let Some(block) = previous_block {
            self.builder.position_at_end(block);
        }
        Ok(function)
    }

    fn compile_conditional_mutable_cell_drop(
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
            .build_struct_gep(cell_type, cell, 1, "mutable.drop.state")
            .map_err(compiler_diagnostic)?;
        let live = self
            .builder
            .build_load(self.context.i8_type(), state, "mutable.drop.live")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let live = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                live,
                self.context.i8_type().const_int(2, false),
                "mutable.drop.is_live",
            )
            .map_err(compiler_diagnostic)?;
        let function = self
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "mutable drop outside a function"))?;
        let drop_block = self.context.append_basic_block(function, "mutable.drop");
        let continue_block = self
            .context
            .append_basic_block(function, "mutable.drop.continue");
        self.builder
            .build_conditional_branch(live, drop_block, continue_block)
            .map_err(compiler_diagnostic)?;
        self.builder.position_at_end(drop_block);
        let slot = self
            .builder
            .build_struct_gep(cell_type, cell, 0, "mutable.drop.value")
            .map_err(compiler_diagnostic)?;
        let value = self
            .builder
            .build_load(llvm_value_type, slot, "mutable.drop.loaded")
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

    fn erased_ref_type(&self) -> inkwell::types::StructType<'context> {
        self.context.struct_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.size_type.into(),
            ],
            false,
        )
    }

    fn coerce_erased_ref_value(
        &self,
        value: AnyValueEnum<'context>,
        source: &CheckedType,
        target: &CheckedType,
        span: Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let (CheckedType::Ref(source), CheckedType::Ref(target)) = (source, target) else {
            return Err(Diagnostic::new(span, "invalid erased reference coercion"));
        };
        let length = erased_ref_length(source, target)
            .filter(|length| *length != usize::MAX)
            .ok_or_else(|| Diagnostic::new(span.clone(), "invalid erased reference coercion"))?;
        let BasicValueEnum::PointerValue(pointer) = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(span.clone(), "invalid fixed reference representation")
        })?
        else {
            return Err(Diagnostic::new(
                span,
                "invalid fixed reference representation",
            ));
        };
        let mut result = self.erased_ref_type().const_zero();
        result = self
            .builder
            .build_insert_value(result, pointer, 0, "erased_ref.pointer")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        result = self
            .builder
            .build_insert_value(
                result,
                self.size_type.const_int(length as u64, false),
                1,
                "erased_ref.length",
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
        let value = self.compile_expression(environment, &index.value)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let position = self.compile_expression(environment, &index.index)?;
        if environment.did_return {
            return Ok(self.unit_value());
        }
        let value = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(
                index.value.syntax().span.clone(),
                "indexed value is not first-class",
            )
        })?;
        let BasicValueEnum::IntValue(position) = value_as_basic(position).ok_or_else(|| {
            Diagnostic::new(
                index.index.syntax().span.clone(),
                "product index is not an integer",
            )
        })?
        else {
            return Err(Diagnostic::new(
                index.index.syntax().span.clone(),
                "product index is not an integer",
            ));
        };
        let checked = self
            .typed_module
            .index_for(index.syntax.id)
            .cloned()
            .ok_or_else(|| Diagnostic::new(index.syntax.span.clone(), "missing checked index"))?;
        let (pointer, length) = match checked.kind {
            crate::CheckedIndexKind::ErasedRef => {
                let BasicValueEnum::StructValue(reference) = value else {
                    return Err(Diagnostic::new(
                        index.value.syntax().span.clone(),
                        "erased reference has an invalid representation",
                    ));
                };
                let pointer = self
                    .builder
                    .build_extract_value(reference, 0, "index.pointer")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let length = self
                    .builder
                    .build_extract_value(reference, 1, "index.length")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                (pointer, length)
            }
            crate::CheckedIndexKind::Ref { length } => {
                let BasicValueEnum::PointerValue(pointer) = value else {
                    return Err(Diagnostic::new(
                        index.value.syntax().span.clone(),
                        "reference has an invalid representation",
                    ));
                };
                (pointer, self.size_type.const_int(length as u64, false))
            }
            crate::CheckedIndexKind::Value { length } => {
                let value_type = value.get_type();
                let pointer = self
                    .builder
                    .build_alloca(value_type, "index.product")
                    .map_err(compiler_diagnostic)?;
                self.builder
                    .build_store(pointer, value)
                    .map_err(compiler_diagnostic)?;
                (pointer, self.size_type.const_int(length as u64, false))
            }
        };
        self.compile_index_load(
            pointer,
            position,
            length,
            checked.element,
            index.syntax.span.clone(),
        )
        .map(|value| value.as_any_value_enum())
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
        if let Some(value) = environment.locals.get(&symbol).copied() {
            return Ok(value);
        }
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let cell_type = self.compile_binding_cell_type(symbol)?;
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
            return self
                .builder
                .build_load(llvm_type, global.as_pointer_value(), "global")
                .map(|value| value.as_any_value_enum())
                .map_err(|error| Diagnostic::new(span, error.to_string()));
        }
        Err(Diagnostic::new(span, unavailable))
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
                .is_some_and(contains_type_parameter);
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
        if let Some(dispatch) = self
            .typed_module
            .trait_dispatch_for(call.callee.syntax().id)
            .cloned()
        {
            let arguments = dispatch
                .arguments
                .into_iter()
                .map(|argument| substitute_type(argument, &self.active_type_substitutions))
                .collect::<Vec<_>>();
            let target = &arguments[0];
            let trait_id = self
                .typed_module
                .resolved()
                .trait_for_method(dispatch.method)
                .expect("trait method owner");
            if self.typed_module.is_default_trait(trait_id)
                && matches!(target, CheckedType::Product(_))
            {
                self.compile_expression(environment, &call.argument)?;
                if environment.did_return {
                    return Ok(self.unit_value());
                }
                return self.compile_default_value(
                    &target,
                    trait_id,
                    dispatch.method,
                    call.syntax.span.clone(),
                );
            }
            let function = self.trait_method_code(
                trait_id,
                &arguments,
                dispatch.method,
                call.callee.syntax().span.clone(),
            )?;
            let expected_count = function.count_params() as usize - 1;
            let mut arguments =
                self.compile_arguments(environment, &call.argument, expected_count, false)?;
            if environment.did_return {
                return Ok(self.unit_value());
            }
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
            let expected_count = function.count_params() as usize - 1;
            let mut arguments =
                self.compile_arguments(environment, &call.argument, expected_count, false)?;
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
            arguments.insert(0, closure_environment.into());
            let call_site = self
                .builder
                .build_direct_call(function, &arguments, "generic.call")
                .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
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
            let scoped_c_string_temporary = !internal
                && self
                    .concrete_expression_type(&call.argument)
                    .is_some_and(|ty| ty == CheckedType::CString)
                && self
                    .typed_module
                    .symbol_for(call.argument.syntax().id)
                    .is_none();
            let expected_count = function.count_params() as usize - usize::from(internal);
            let mut arguments = self.compile_arguments(
                environment,
                &call.argument,
                expected_count,
                function.get_type().is_var_arg(),
            )?;
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
                arguments.insert(0, closure_environment.into());
            }
            let call_site = self
                .builder
                .build_direct_call(function, &arguments, "call")
                .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
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
        let expected_count = self
            .compile_parameter_types(&function_type.parameter)?
            .len();
        let mut arguments =
            self.compile_arguments(environment, &call.argument, expected_count, false)?;
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
        Ok(call_site
            .try_as_basic_value()
            .unwrap_basic()
            .as_any_value_enum())
    }

    fn compile_intrinsic_call(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        call: &CallExpression,
        intrinsic: IntrinsicFunction,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        match intrinsic {
            IntrinsicFunction::StringFromCString => {
                return self.compile_string_from_c_string(environment, call);
            }
            IntrinsicFunction::StringToCString => {
                return self.compile_string_to_c_string(environment, call);
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
            IntrinsicFunction::ErasedProductLength => {
                let value = self.compile_expression(environment, &call.argument)?;
                let Some(BasicValueEnum::StructValue(reference)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        call.argument.syntax().span.clone(),
                        "length requires an erased product reference",
                    ));
                };
                return self
                    .builder
                    .build_extract_value(reference, 1, "erased_ref.length")
                    .map(|value| value.as_any_value_enum())
                    .map_err(compiler_diagnostic);
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
            | IntrinsicFunction::FloatBinary { .. }
            | IntrinsicFunction::FloatCompare { .. }
            | IntrinsicFunction::ErasedProductLength
            | IntrinsicFunction::RefReplace
            | IntrinsicFunction::Drop => {
                unreachable!()
            }
        }
        .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        Ok(value.as_any_value_enum())
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
            .functions()
            .iter()
            .find(|function| function.id == function_id)
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
                    || self.typed_module.resolved().is_mutable_symbol(symbol)
                {
                    environment
                        .binding_cells
                        .get(&symbol)
                        .copied()
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
                || self.typed_module.resolved().is_mutable_symbol(symbol)
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
                    || self.typed_module.resolved().is_mutable_symbol(*symbol)
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

    fn compile_product_elements(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        product: &ProductExpression,
    ) -> CodeGenerationResult<Vec<BasicValueEnum<'context>>> {
        let mut values = Vec::new();
        for element in &product.elements {
            let value = self.compile_expression(environment, &element.value)?;
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
        Ok(values)
    }

    fn compile_arguments(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        argument: &Expression,
        expected_count: usize,
        variadic: bool,
    ) -> CodeGenerationResult<Vec<inkwell::values::BasicMetadataValueEnum<'context>>> {
        let arguments = if let Expression::Product(product) = argument {
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
        parameter_types.extend(self.compile_parameter_types(&function_type.parameter)?);
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
            CheckedType::CChar => Ok(self.context.i8_type().into()),
            CheckedType::Parameter { name, .. } => Err(Diagnostic::new(
                Span::Compiler,
                format!("cannot generate code for unspecialized type parameter `{name}`"),
            )),
            CheckedType::TypeConstructor { name, .. } => Err(Diagnostic::new(
                Span::Compiler,
                format!("cannot generate code for partially applied type `{name}`"),
            )),
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
            CheckedType::Ref(payload)
                if matches!(payload.as_ref(), CheckedType::ErasedProduct(_)) =>
            {
                Ok(self.erased_ref_type().into())
            }
            CheckedType::Ref(_) => Ok(self.context.ptr_type(AddressSpace::default()).into()),
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
        CheckedType::Ref(_) => true,
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

fn compiler_diagnostic(error: inkwell::builder::BuilderError) -> Diagnostic {
    Diagnostic::new(Span::Compiler, error.to_string())
}
