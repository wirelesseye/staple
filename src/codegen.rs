use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    values::{AnyValue, AnyValueEnum, BasicValueEnum},
};

use crate::{
    Accessor, BinaryExpression, BinaryOperator, CheckedFunctionType, CheckedProductType,
    CheckedType, Diagnostic, Expression, FunctionId, Item, Pattern, ProductExpression,
    ResolvedFunction, Span, Statement, SymbolId, TypedModule,
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
    globals: HashMap<SymbolId, inkwell::values::AnyValueEnum<'context>>,
}

#[derive(Default)]
struct FunctionEnvironment<'context> {
    locals: HashMap<SymbolId, inkwell::values::AnyValueEnum<'context>>,
}

type CodeGenerationResult<T> = Result<T, Diagnostic>;

impl<'module, 'context> ModuleEmitter<'module, 'context> {
    fn new(
        context: &'context inkwell::context::Context,
        typed_module: &'module TypedModule,
    ) -> Self {
        Self {
            context,
            typed_module,
            llvm_module: context.create_module("staple"),
            builder: context.create_builder(),
            functions: HashMap::new(),
            globals: HashMap::new(),
        }
    }

    fn lookup_value(
        &self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
    ) -> Option<inkwell::values::AnyValueEnum<'context>> {
        environment
            .locals
            .get(&symbol)
            .or_else(|| self.globals.get(&symbol))
            .copied()
    }
}

impl<'context> CodeGenerator<'context> {
    pub fn new(context: &'context inkwell::context::Context) -> Self {
        Self { context }
    }

    pub fn compile_module(&self, module: &TypedModule) -> Result<String, Vec<Diagnostic>> {
        ModuleEmitter::new(self.context, module)
            .compile()
            .map_err(|diagnostic| vec![diagnostic])
    }
}

impl<'module, 'context> ModuleEmitter<'module, 'context> {
    fn compile(mut self) -> CodeGenerationResult<String> {
        self.declare_external_functions()?;
        self.declare_functions()?;
        let typed_module = self.typed_module;
        for function in typed_module.functions() {
            self.compile_function_body(function)?;
        }
        self.compile_main_function()?;

        self.llvm_module.verify().map_err(|message| {
            Diagnostic::new(Span::Compiler, format!("invalid LLVM module: {message}"))
        })?;
        Ok(self.llvm_module.print_to_string().to_string())
    }

    fn declare_external_functions(&mut self) -> CodeGenerationResult<()> {
        for item in &self.typed_module.syntax().items {
            let Item::ExternBlock(block) = item else {
                continue;
            };
            for binding in &block.bindings {
                let symbol = self
                    .typed_module
                    .symbol_for(binding.syntax.id)
                    .ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "unresolved external binding")
                    })?;
                let Some(CheckedType::Function(function_type)) =
                    self.typed_module.type_of_symbol(symbol)
                else {
                    return Err(Diagnostic::new(
                        binding.syntax.span.clone(),
                        "external bindings must have a function type",
                    ));
                };
                let llvm_type = self.compile_function_type(function_type)?;
                let function = self
                    .llvm_module
                    .add_function(&binding.name, llvm_type, None);
                self.globals.insert(symbol, function.into());
            }
        }
        Ok(())
    }

    fn declare_functions(&mut self) -> CodeGenerationResult<()> {
        for function in self.typed_module.functions() {
            let function_type =
                self.typed_module
                    .type_of_function(function.id)
                    .ok_or_else(|| {
                        Diagnostic::new(function.body.syntax().span.clone(), "unchecked function")
                    })?;
            let llvm_type = self.compile_function_type(function_type)?;
            let llvm_function = self
                .llvm_module
                .add_function(&function.name, llvm_type, None);
            self.functions.insert(function.id, llvm_function);
            if let Some(binding_syntax) = function.binding_syntax
                && let Some(symbol) = self.typed_module.symbol_for(binding_syntax)
            {
                self.globals.insert(symbol, llvm_function.into());
            }
        }
        Ok(())
    }

    fn compile_function_body(&mut self, function: &ResolvedFunction) -> CodeGenerationResult<()> {
        let llvm_function = self.functions[&function.id];
        let entry = self.context.append_basic_block(llvm_function, "entry");
        self.builder.position_at_end(entry);

        let mut environment = FunctionEnvironment::default();
        self.bind_function_parameters(&mut environment, function, llvm_function)?;
        let value = self.compile_expression(&mut environment, &function.body)?;
        let return_value = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(
                function.body.syntax().span.clone(),
                "function result is not a first-class value",
            )
        })?;
        self.builder
            .build_return(Some(&return_value))
            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        Ok(())
    }

    fn bind_function_parameters(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        function: &ResolvedFunction,
        llvm_function: inkwell::values::FunctionValue<'context>,
    ) -> CodeGenerationResult<()> {
        let parameters = llvm_function.get_params();
        self.bind_top_level_pattern(environment, &function.pattern, &parameters)
    }

    fn bind_top_level_pattern(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        pattern: &Pattern,
        values: &[BasicValueEnum<'context>],
    ) -> CodeGenerationResult<()> {
        match pattern {
            Pattern::Binding(_) => {
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
        }
    }

    fn compile_main_function(&mut self) -> CodeGenerationResult<()> {
        let integer_type = self.context.i32_type();
        let function_type = integer_type.fn_type(&[], false);
        let function = self.llvm_module.add_function("main", function_type, None);
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);

        let mut environment = FunctionEnvironment::default();
        let typed_module = self.typed_module;
        for item in &typed_module.syntax().items {
            if let Item::Statement(statement) = item {
                self.compile_statement(&mut environment, statement)?;
            }
        }
        self.builder
            .build_return(Some(&integer_type.const_zero()))
            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        Ok(())
    }

    fn compile_statement(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        statement: &Statement,
    ) -> CodeGenerationResult<Option<AnyValueEnum<'context>>> {
        match statement {
            Statement::Binding(binding) => {
                if let Some(expression) = &binding.value {
                    let value = self.compile_expression(environment, expression)?;
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
            Statement::Expression(expression) => {
                self.compile_expression(environment, expression).map(Some)
            }
        }
    }

    fn compile_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        expression: &Expression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
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
                Ok(self.functions[&id].into())
            }
            Expression::Block(block) => {
                let mut value = None;
                for statement in &block.statements {
                    value = self.compile_statement(environment, statement)?;
                }
                Ok(value.unwrap_or_else(|| {
                    self.context
                        .struct_type(&[], false)
                        .const_zero()
                        .as_any_value_enum()
                }))
            }
            Expression::Product(product) => self.compile_product_expression(environment, product),
            Expression::Call(call) => {
                let callee = self.compile_expression(environment, &call.callee)?;
                let AnyValueEnum::FunctionValue(function) = callee else {
                    return Err(Diagnostic::new(
                        call.callee.syntax().span.clone(),
                        "expression is not directly callable",
                    ));
                };
                let arguments = self.compile_arguments(
                    environment,
                    &call.argument,
                    function.count_params() as usize,
                )?;
                let call_site = self
                    .builder
                    .build_direct_call(function, &arguments, "call")
                    .map_err(|error| {
                        Diagnostic::new(call.syntax.span.clone(), error.to_string())
                    })?;
                Ok(call_site
                    .try_as_basic_value()
                    .unwrap_basic()
                    .as_any_value_enum())
            }
            Expression::Access(access) => {
                let value = self.compile_expression(environment, &access.value)?;
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
            Expression::Binary(binary) => self.compile_binary_expression(environment, binary),
            Expression::Name(name) => {
                let symbol = self
                    .typed_module
                    .symbol_for(name.syntax.id)
                    .ok_or_else(|| Diagnostic::new(name.syntax.span.clone(), "unresolved name"))?;
                self.lookup_value(environment, symbol).ok_or_else(|| {
                    Diagnostic::new(
                        name.syntax.span.clone(),
                        format!("value `{}` is not available here", name.name),
                    )
                })
            }
            Expression::String(string) => {
                let value = decode_string_literal(&string.literal)
                    .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
                self.builder
                    .build_global_string_ptr(&value, "string")
                    .map(|global| global.as_any_value_enum())
                    .map_err(|error| Diagnostic::new(string.syntax.span.clone(), error.to_string()))
            }
            Expression::Integer(integer) => {
                let value = integer.literal.parse::<u64>().map_err(|_| {
                    Diagnostic::new(integer.syntax.span.clone(), "integer literal is too large")
                })?;
                Ok(self
                    .context
                    .i32_type()
                    .const_int(value, false)
                    .as_any_value_enum())
            }
        }
    }

    fn compile_binary_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        binary: &BinaryExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let AnyValueEnum::IntValue(left) = self.compile_expression(environment, &binary.left)?
        else {
            return Err(Diagnostic::new(
                binary.left.syntax().span.clone(),
                "arithmetic operands must be integers",
            ));
        };
        let AnyValueEnum::IntValue(right) = self.compile_expression(environment, &binary.right)?
        else {
            return Err(Diagnostic::new(
                binary.right.syntax().span.clone(),
                "arithmetic operands must be integers",
            ));
        };
        let result = match binary.operator {
            BinaryOperator::Add => self.builder.build_int_add(left, right, "add"),
            BinaryOperator::Subtract => self.builder.build_int_sub(left, right, "subtract"),
            BinaryOperator::Multiply => self.builder.build_int_mul(left, right, "multiply"),
            BinaryOperator::Divide => self.builder.build_int_signed_div(left, right, "divide"),
        }
        .map_err(|error| Diagnostic::new(binary.syntax.span.clone(), error.to_string()))?;
        Ok(result.as_any_value_enum())
    }

    fn compile_product_expression(
        &mut self,
        environment: &mut FunctionEnvironment<'context>,
        product: &ProductExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let values = product
            .elements
            .iter()
            .map(|element| {
                self.compile_expression(environment, &element.value)
                    .and_then(|value| {
                        value_as_basic(value).ok_or_else(|| {
                            Diagnostic::new(
                                element.syntax.span.clone(),
                                "product element is not a first-class value",
                            )
                        })
                    })
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
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
    ) -> CodeGenerationResult<Vec<inkwell::values::BasicMetadataValueEnum<'context>>> {
        let expressions: Vec<&Expression> = match argument {
            Expression::Product(product) => product
                .elements
                .iter()
                .map(|element| &element.value)
                .collect(),
            expression => vec![expression],
        };
        let arguments = expressions
            .into_iter()
            .map(|expression| {
                self.compile_expression(environment, expression)
                    .and_then(|value| {
                        value_as_basic(value).ok_or_else(|| {
                            Diagnostic::new(
                                expression.syntax().span.clone(),
                                "argument is not a first-class value",
                            )
                        })
                    })
            })
            .collect::<CodeGenerationResult<Vec<BasicValueEnum<'context>>>>()?;
        if arguments.len() == expected_count {
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

    fn compile_function_type(
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
            CheckedType::String => Ok(self.context.ptr_type(AddressSpace::default()).into()),
            CheckedType::Pointer { .. } | CheckedType::Function(_) => {
                Ok(self.context.ptr_type(AddressSpace::default()).into())
            }
            CheckedType::Product(product) => self.compile_product_type(product).map(Into::into),
            CheckedType::I32 => Ok(self.context.i32_type().into()),
            CheckedType::Bool => Ok(self.context.bool_type().into()),
            CheckedType::Distinct { representation, .. } => self.compile_type(representation),
        }
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

#[cfg(test)]
mod tests {
    use super::decode_string_literal;

    #[test]
    fn decodes_string_quotes_and_escapes() {
        assert_eq!(decode_string_literal("\"hello\\n\"").unwrap(), "hello\n");
    }
}
