use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use inkwell::{
    AddressSpace, OptimizationLevel,
    module::Module as LlvmModule,
    targets::TargetData,
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
    },
    types::BasicType,
    values::{AnyValue, AnyValueEnum, BasicValue, BasicValueEnum},
};

use crate::typecheck::{contains_type_parameter, infer_type_parameters, substitute_type};
use crate::{
    Accessor, CallExpression, CheckedFunctionType, CheckedProductType, CheckedType, Diagnostic,
    Expression, FunctionId, IntegerBinaryOperation, IntegerCompareOperation, IntegerType,
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
    external_symbols: HashSet<SymbolId>,
    storage: HashMap<SymbolId, inkwell::values::GlobalValue<'context>>,
    initializers: HashMap<ModuleId, inkwell::values::FunctionValue<'context>>,
    size_type: inkwell::types::IntType<'context>,
    target_data: TargetData,
}

#[derive(Default)]
struct FunctionEnvironment<'context> {
    locals: HashMap<SymbolId, inkwell::values::AnyValueEnum<'context>>,
    function_id: Option<FunctionId>,
    closure_environment: Option<inkwell::values::PointerValue<'context>>,
    did_return: bool,
}

type CodeGenerationResult<T> = Result<T, Diagnostic>;

impl<'module, 'context> ModuleEmitter<'module, 'context> {
    fn new(
        context: &'context inkwell::context::Context,
        typed_module: &'module TypedModule,
        target_machine: &TargetMachine,
    ) -> Self {
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
            external_symbols: HashSet::new(),
            storage: HashMap::new(),
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
                if !binding.type_parameters.is_empty() {
                    continue;
                }
                let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) else {
                    continue;
                };
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
            Pattern::Wildcard(_) => {}
            Pattern::Binding(binding) => {
                let Some(symbol) = self.typed_module.symbol_for(binding.syntax.id) else {
                    return Ok(());
                };
                let Some(value_type) = self.typed_module.type_of_symbol(symbol) else {
                    return Ok(());
                };
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
            if let Some(binding_syntax) = function.binding_syntax
                && let Some(symbol) = self.typed_module.symbol_for(binding_syntax)
            {
                self.globals.insert(symbol, llvm_function.into());
                self.closure_codes.insert(symbol, llvm_function);
            }
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
        let value = self.compile_expression(&mut environment, &function.body)?;
        if !environment.did_return {
            let return_value = value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    function.body.syntax().span.clone(),
                    "function result is not a first-class value",
                )
            })?;
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
                environment.locals.insert(symbol, value.as_any_value_enum());
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
            Pattern::Wildcard(_) => Ok(()),
            Pattern::Binding(binding) => {
                value.set_name(&binding.name);
                let symbol = self
                    .typed_module
                    .symbol_for(binding.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "unresolved pattern binding")
                    })?;
                environment.locals.insert(symbol, value.as_any_value_enum());
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
                self.bind_pattern_value(environment, &pattern.argument, value)
            }
        }
    }

    fn compile_main_function(&mut self) -> CodeGenerationResult<()> {
        let integer_type = self.context.i32_type();
        let function_type = integer_type.fn_type(&[], false);
        let function = self.llvm_module.add_function("main", function_type, None);
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);

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
                if !binding.type_parameters.is_empty() {
                    return Ok(());
                }
                let Some(expression) = &binding.value else {
                    return Ok(());
                };
                let value = self.compile_expression(environment, expression)?;
                let symbol = self
                    .typed_module
                    .symbol_for(binding.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "unresolved binding")
                    })?;
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
                Ok(())
            }
            Statement::PatternBinding(binding) => {
                let value = self.compile_expression(environment, &binding.value)?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        binding.syntax.span.clone(),
                        "destructured value is not first-class",
                    )
                })?;
                self.bind_pattern_value(environment, &binding.pattern, value)?;
                self.store_pattern_globals(environment, &binding.pattern)
            }
            Statement::Return(statement) => Err(Diagnostic::new(
                statement.syntax.span.clone(),
                "`return` is only allowed inside a function",
            )),
            Statement::Expression(expression) => {
                self.compile_expression(environment, expression)?;
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
            Pattern::Wildcard(_) => {}
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

    fn compile_statement(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        statement: &Statement,
    ) -> CodeGenerationResult<Option<AnyValueEnum<'context>>> {
        match statement {
            Statement::Binding(binding) => {
                if !binding.type_parameters.is_empty() {
                    return Ok(None);
                }
                if let Some(expression) = &binding.value {
                    let value = self.compile_expression(environment, expression)?;
                    if environment.did_return {
                        return Ok(None);
                    }
                    let symbol =
                        self.typed_module
                            .symbol_for(binding.syntax.id)
                            .ok_or_else(|| {
                                Diagnostic::new(binding.syntax.span.clone(), "unresolved binding")
                            })?;
                    environment.locals.insert(symbol, value);
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
                self.builder
                    .build_return(Some(&value))
                    .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
                environment.did_return = true;
                Ok(None)
            }
            Statement::Expression(expression) => {
                self.compile_expression(environment, expression).map(Some)
            }
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
            return Ok(value);
        }
        let Some(coercion) = self
            .typed_module
            .coercion_for(expression.syntax().id)
            .cloned()
        else {
            return Ok(value);
        };
        let source = substitute_type(coercion.source, &self.active_type_substitutions);
        let target = substitute_type(coercion.target, &self.active_type_substitutions);
        self.coerce_sum_value(value, &source, &target, expression.syntax().span.clone())
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
            let target = substitute_type(dispatch.target, &self.active_type_substitutions);
            if contains_type_parameter(&target) {
                return Err(Diagnostic::new(
                    expression.syntax().span.clone(),
                    "trait method target is not fully specialized",
                ));
            }
            let trait_id = self
                .typed_module
                .resolved()
                .trait_for_method(dispatch.method)
                .expect("trait method owner");
            let function_id = self
                .typed_module
                .trait_impl_method(trait_id, &target, dispatch.method)
                .ok_or_else(|| {
                    Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!("no trait implementation is available for `{target}`"),
                    )
                })?;
            return self
                .build_closure(environment, function_id, expression.syntax().span.clone())
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
            Expression::Match(match_) => self.compile_match_expression(environment, match_),
            Expression::Block(block) => {
                let mut value = None;
                for statement in &block.statements {
                    value = self.compile_statement(environment, statement)?;
                    if environment.did_return {
                        break;
                    }
                }
                Ok(value.unwrap_or_else(|| self.unit_value()))
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
                    && self
                        .typed_module
                        .resolved()
                        .constructor_type(symbol)
                        .is_some()
                {
                    let value = self.compile_expression(environment, &call.argument)?;
                    return Ok(if environment.did_return {
                        self.unit_value()
                    } else {
                        value
                    });
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
                        access.syntax.span.clone(),
                        "value is not available here".to_owned(),
                    );
                }
                let value = self.compile_expression(environment, &access.value)?;
                if environment.did_return {
                    return Ok(self.unit_value());
                }
                let Some(BasicValueEnum::StructValue(value)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        access.value.syntax().span.clone(),
                        "element access requires a product value",
                    ));
                };
                let index = match &access.accessor {
                    Accessor::Index(index) => index.parse::<u32>().map_err(|_| {
                        Diagnostic::new(access.syntax.span.clone(), "invalid product index")
                    })?,
                    Accessor::Name(name) => {
                        product_element_index(&access.value, name).ok_or_else(|| {
                            Diagnostic::new(
                                access.syntax.span.clone(),
                                format!("cannot determine index of product element `{name}`"),
                            )
                        })?
                    }
                };
                self.builder
                    .build_extract_value(value, index, "element")
                    .map(|value| value.as_any_value_enum())
                    .map_err(|error| Diagnostic::new(access.syntax.span.clone(), error.to_string()))
            }
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
                    name.syntax.span.clone(),
                    format!("value `{}` is not available here", name.name),
                )
            }
            Expression::String(string) => {
                let value = decode_string_literal(&string.literal)
                    .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
                let source = self
                    .builder
                    .build_global_string_ptr(&value, "string")
                    .map_err(|error| {
                        Diagnostic::new(string.syntax.span.clone(), error.to_string())
                    })?
                    .as_pointer_value();
                let length = self.size_type.const_int(value.len() as u64, false);
                let pointer = self
                    .builder
                    .build_array_malloc(self.context.i8_type(), length, "string.data")
                    .map_err(|error| {
                        Diagnostic::new(string.syntax.span.clone(), error.to_string())
                    })?;
                self.builder
                    .build_memcpy(pointer, 1, source, 1, length)
                    .map_err(|error| {
                        Diagnostic::new(string.syntax.span.clone(), error.to_string())
                    })?;
                self.build_string_value(pointer, length, string.syntax.span.clone())
                    .map(|value| value.as_any_value_enum())
            }
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
        }
    }

    fn unit_value(&self) -> AnyValueEnum<'context> {
        self.context
            .struct_type(&[], true)
            .const_zero()
            .as_any_value_enum()
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
        for arm in &match_.arms {
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
                self.builder
                    .build_unconditional_branch(merge_block)
                    .map_err(|error| Diagnostic::new(arm.syntax.span.clone(), error.to_string()))?;
                let predecessor = self.builder.get_insert_block().expect("match arm block");
                incoming.push((value, predecessor));
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
            return Ok(self.unit_value());
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
            Pattern::Binding(_) | Pattern::Wildcard(_) => {
                self.bind_pattern_value(environment, pattern, value)?;
                self.builder
                    .build_unconditional_branch(success)
                    .map_err(compiler_diagnostic)?;
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

        match source {
            CheckedType::Distinct { .. } => {
                let index = target_sum
                    .alternatives
                    .iter()
                    .position(|alternative| alternative == source)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            span.clone(),
                            "sum injection target is missing its source alternative",
                        )
                    })?;
                self.builder
                    .build_store(
                        target_tag,
                        self.context.i32_type().const_int(index as u64, false),
                    )
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                let source_type = self.compile_type(source)?;
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
                        target_payload,
                        target_alignment,
                        source_slot,
                        self.target_data.get_abi_alignment(&source_type),
                        size,
                    )
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            }
            CheckedType::Sum(source_sum) => {
                let source_type = self.compile_sum_type(source_sum)?;
                let source_value = value_as_basic(value)
                    .and_then(|value| match value {
                        BasicValueEnum::StructValue(value) => Some(value),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "sum value has an invalid representation")
                    })?;
                let source_slot = self
                    .builder
                    .build_alloca(source_type, "sum.source")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                self.builder
                    .build_store(source_slot, source_value)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                let source_payload = self
                    .builder
                    .build_struct_gep(source_type, source_slot, 1, "sum.source.payload")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                let source_payload_type = source_type
                    .get_field_type_at_index(1)
                    .expect("sum payload field");
                let source_tag = self
                    .builder
                    .build_extract_value(source_value, 0, "sum.source.tag")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                    .into_int_value();
                let mut translated = self.context.i32_type().const_zero();
                for (source_index, alternative) in source_sum.alternatives.iter().enumerate() {
                    let Some(target_index) = target_sum
                        .alternatives
                        .iter()
                        .position(|candidate| candidate == alternative)
                    else {
                        continue;
                    };
                    let matches = self
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::EQ,
                            source_tag,
                            self.context
                                .i32_type()
                                .const_int(source_index as u64, false),
                            "sum.tag.matches",
                        )
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                    translated = self
                        .builder
                        .build_select(
                            matches,
                            self.context
                                .i32_type()
                                .const_int(target_index as u64, false),
                            translated,
                            "sum.tag.translate",
                        )
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                        .into_int_value();
                }
                self.builder
                    .build_store(target_tag, translated)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                self.builder
                    .build_memcpy(
                        target_payload,
                        target_alignment,
                        source_payload,
                        self.target_data.get_abi_alignment(&source_payload_type),
                        self.size_type.const_int(
                            self.target_data.get_store_size(&source_payload_type),
                            false,
                        ),
                    )
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            }
            _ => return Err(Diagnostic::new(span, "invalid sum coercion source")),
        }
        self.builder
            .build_load(target_type, target_slot, "sum.value")
            .map(|value| value.as_any_value_enum())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
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
        let value = decode_string_literal(&string.literal)
            .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
        if value.as_bytes().contains(&0) {
            return Err(Diagnostic::new(
                string.syntax.span.clone(),
                "C string literals cannot contain an interior NUL byte",
            ));
        }
        self.builder
            .build_global_string_ptr(&value, "c_string")
            .map(|global| global.as_any_value_enum())
            .map_err(|error| Diagnostic::new(string.syntax.span.clone(), error.to_string()))
    }

    fn build_string_value(
        &self,
        pointer: inkwell::values::PointerValue<'context>,
        length: inkwell::values::IntValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let mut value = self.string_type().const_zero();
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
        self.builder
            .build_insert_value(value, length, 2, "string.capacity")
            .map(|value| value.into_struct_value())
            .map_err(|error| Diagnostic::new(span, error.to_string()))
    }

    fn compile_symbol_value(
        &mut self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: Span,
        unavailable: String,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if let Some(value) = environment.locals.get(&symbol).copied() {
            return Ok(value);
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
        if let Some(global) = self.storage.get(&symbol) {
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
            let target = substitute_type(dispatch.target, &self.active_type_substitutions);
            let trait_id = self
                .typed_module
                .resolved()
                .trait_for_method(dispatch.method)
                .expect("trait method owner");
            let function_id = self
                .typed_module
                .trait_impl_method(trait_id, &target, dispatch.method)
                .ok_or_else(|| {
                    Diagnostic::new(
                        call.callee.syntax().span.clone(),
                        format!("no trait implementation is available for `{target}`"),
                    )
                })?;
            let function = self.functions[&function_id];
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
            _ => {}
        }
        let arguments = self.compile_arguments(environment, &call.argument, 2, false)?;
        if environment.did_return {
            return Ok(self.unit_value());
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
            IntrinsicFunction::StringFromCString | IntrinsicFunction::StringToCString => {
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
        let pointer = self
            .builder
            .build_array_malloc(self.context.i8_type(), length, "string.data")
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.builder
            .build_memcpy(pointer, 1, source, 1, length)
            .map_err(|error| Diagnostic::new(call.syntax.span.clone(), error.to_string()))?;
        self.build_string_value(pointer, length, call.syntax.span.clone())
            .map(|value| value.as_any_value_enum())
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
                let value = self.compile_symbol_value(
                    environment,
                    symbol,
                    span.clone(),
                    "captured value is not available here".to_owned(),
                )?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "captured value is not first-class")
                })?;
                environment_value = self
                    .builder
                    .build_insert_value(environment_value, value, index as u32, "capture")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                    .into_struct_value();
            }
            let pointer = self
                .builder
                .build_malloc(environment_type, "closure.environment")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            self.builder
                .build_store(pointer, environment_value)
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            pointer
        };
        self.build_closure_value(code, environment_pointer)
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
        let mut values = Vec::new();
        for element in &product.elements {
            let value = self.compile_expression(environment, &element.value)?;
            if environment.did_return {
                return Ok(self.unit_value());
            }
            values.push(value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    element.syntax.span.clone(),
                    "product element is not a first-class value",
                )
            })?);
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

    fn compile_arguments(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        argument: &Expression,
        expected_count: usize,
        variadic: bool,
    ) -> CodeGenerationResult<Vec<inkwell::values::BasicMetadataValueEnum<'context>>> {
        let expressions: Vec<&Expression> = match argument {
            Expression::Product(product) => product
                .elements
                .iter()
                .map(|element| &element.value)
                .collect(),
            expression => vec![expression],
        };
        let mut arguments = Vec::new();
        for expression in expressions {
            let value = self.compile_expression(environment, expression)?;
            if environment.did_return {
                return Ok(Vec::new());
            }
            arguments.push(value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    expression.syntax().span.clone(),
                    "argument is not a first-class value",
                )
            })?);
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
            CheckedType::String => Ok(self.string_type().into()),
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

    fn closure_type(&self) -> inkwell::types::StructType<'context> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        self.context
            .struct_type(&[pointer.into(), pointer.into()], false)
    }

    fn string_type(&self) -> inkwell::types::StructType<'context> {
        self.context.struct_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.size_type.into(),
                self.size_type.into(),
            ],
            false,
        )
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

fn product_element_index(expression: &Expression, name: &str) -> Option<u32> {
    let Expression::Product(product) = expression else {
        return None;
    };
    product
        .elements
        .iter()
        .position(|element| element.name.as_deref() == Some(name))
        .and_then(|index| u32::try_from(index).ok())
}

fn decode_string_literal(literal: &str) -> Result<String, String> {
    let content = literal
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| "unterminated string literal".to_owned())?;
    let mut output = String::new();
    let mut characters = content.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let escaped = characters
            .next()
            .ok_or_else(|| "unterminated string escape".to_owned())?;
        output.push(match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            '\\' => '\\',
            '"' => '"',
            other => return Err(format!("unknown string escape `\\{other}`")),
        });
    }
    Ok(output)
}

fn compiler_diagnostic(error: inkwell::builder::BuilderError) -> Diagnostic {
    Diagnostic::new(Span::Compiler, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::decode_string_literal;

    #[test]
    fn decodes_string_quotes_and_escapes() {
        assert_eq!(decode_string_literal("\"hello\\n\"").unwrap(), "hello\n");
    }
}
