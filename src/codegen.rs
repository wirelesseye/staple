use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    values::{AnyValue, AnyValueEnum, BasicValueEnum},
};

use crate::{
    Accessor, BinaryExpression, BinaryOperator, CheckedFunctionType, CheckedListType, CheckedType,
    Diagnostic, Expression, FunctionId, Item, ListExpression, Parameter, ResolvedFunction, Span,
    Statement, SymbolId, TypedModule,
};

pub struct CodeGenerator<'context> {
    context: &'context inkwell::context::Context,
    builder: inkwell::builder::Builder<'context>,
}

struct ModuleContext<'context> {
    functions: HashMap<FunctionId, inkwell::values::FunctionValue<'context>>,
    values: HashMap<SymbolId, inkwell::values::AnyValueEnum<'context>>,
}

type CodeGenerationResult<T> = Result<T, Diagnostic>;

impl<'context> CodeGenerator<'context> {
    pub fn new(context: &'context inkwell::context::Context) -> Self {
        Self {
            context,
            builder: context.create_builder(),
        }
    }

    pub fn compile_module(&self, module: &TypedModule) -> Result<String, Vec<Diagnostic>> {
        self.compile_module_inner(module)
            .map_err(|diagnostic| vec![diagnostic])
    }

    fn compile_module_inner(&self, module: &TypedModule) -> CodeGenerationResult<String> {
        let llvm_module = self.context.create_module("staple");
        let mut module_context = ModuleContext {
            functions: HashMap::new(),
            values: HashMap::new(),
        };

        self.declare_external_functions(&llvm_module, &mut module_context, module)?;
        self.declare_functions(&llvm_module, &mut module_context, module)?;
        for function in module.functions() {
            self.compile_function_body(&llvm_module, &mut module_context, module, function)?;
        }
        self.compile_main_function(&llvm_module, &mut module_context, module)?;

        llvm_module.verify().map_err(|message| {
            Diagnostic::new(Span::Compiler, format!("invalid LLVM module: {message}"))
        })?;
        Ok(llvm_module.print_to_string().to_string())
    }

    fn declare_external_functions(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
    ) -> CodeGenerationResult<()> {
        for item in &module.syntax().items {
            let Item::ExternBlock(block) = item else {
                continue;
            };
            for binding in &block.bindings {
                let symbol = module.symbol_for(binding.syntax.id).ok_or_else(|| {
                    Diagnostic::new(binding.syntax.span.clone(), "unresolved external binding")
                })?;
                let Some(CheckedType::Function(function_type)) = module.type_of_symbol(symbol)
                else {
                    return Err(Diagnostic::new(
                        binding.syntax.span.clone(),
                        "external bindings must have a function type",
                    ));
                };
                let llvm_type = self.compile_function_type(function_type)?;
                let function = llvm_module.add_function(&binding.name, llvm_type, None);
                module_context.values.insert(symbol, function.into());
            }
        }
        Ok(())
    }

    fn declare_functions(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
    ) -> CodeGenerationResult<()> {
        for function in module.functions() {
            let function_type = module.type_of_function(function.id).ok_or_else(|| {
                Diagnostic::new(function.body.syntax().span.clone(), "unchecked function")
            })?;
            let llvm_type = self.compile_function_type(function_type)?;
            let llvm_function = llvm_module.add_function(&function.name, llvm_type, None);
            module_context.functions.insert(function.id, llvm_function);
            if let Some(binding_syntax) = function.binding_syntax
                && let Some(symbol) = module.symbol_for(binding_syntax)
            {
                module_context.values.insert(symbol, llvm_function.into());
            }
        }
        Ok(())
    }

    fn compile_function_body(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
        function: &ResolvedFunction,
    ) -> CodeGenerationResult<()> {
        let llvm_function = module_context.functions[&function.id];
        let entry = self.context.append_basic_block(llvm_function, "entry");
        self.builder.position_at_end(entry);

        self.bind_function_parameters(module_context, module, function, llvm_function)?;
        let value = self.compile_expression(llvm_module, module_context, module, &function.body)?;
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
        &self,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
        function: &ResolvedFunction,
        llvm_function: inkwell::values::FunctionValue<'context>,
    ) -> CodeGenerationResult<()> {
        let parameters = llvm_function.get_params();
        let syntax_parameters = match &function.parameter {
            Parameter::Value(value) => vec![value],
            Parameter::List(list) => list.elements.iter().collect(),
        };
        if parameters.len() != syntax_parameters.len() {
            return Err(Diagnostic::new(
                function.parameter.ty().syntax().span.clone(),
                "function parameter layout does not match its declared type",
            ));
        }
        for (value, parameter) in parameters.into_iter().zip(syntax_parameters) {
            value.set_name(&parameter.name);
            let symbol = module.symbol_for(parameter.syntax.id).ok_or_else(|| {
                Diagnostic::new(
                    parameter.syntax.span.clone(),
                    "unresolved function parameter",
                )
            })?;
            module_context
                .values
                .insert(symbol, value.as_any_value_enum());
        }
        Ok(())
    }

    fn compile_main_function(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
    ) -> CodeGenerationResult<()> {
        let integer_type = self.context.i32_type();
        let function_type = integer_type.fn_type(&[], false);
        let function = llvm_module.add_function("main", function_type, None);
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);

        for item in &module.syntax().items {
            if let Item::Statement(statement) = item {
                self.compile_statement(llvm_module, module_context, module, statement)?;
            }
        }
        self.builder
            .build_return(Some(&integer_type.const_zero()))
            .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
        Ok(())
    }

    fn compile_statement(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
        statement: &Statement,
    ) -> CodeGenerationResult<Option<AnyValueEnum<'context>>> {
        match statement {
            Statement::Binding(binding) => {
                if let Some(expression) = &binding.value {
                    let value =
                        self.compile_expression(llvm_module, module_context, module, expression)?;
                    let symbol = module.symbol_for(binding.syntax.id).ok_or_else(|| {
                        Diagnostic::new(binding.syntax.span.clone(), "unresolved binding")
                    })?;
                    module_context.values.insert(symbol, value);
                }
                Ok(None)
            }
            Statement::Expression(expression) => self
                .compile_expression(llvm_module, module_context, module, expression)
                .map(Some),
        }
    }

    fn compile_expression(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
        expression: &Expression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        match expression {
            Expression::Function(function) => {
                let id = module.function_for(function.syntax.id).ok_or_else(|| {
                    Diagnostic::new(
                        function.syntax.span.clone(),
                        "unresolved function expression",
                    )
                })?;
                Ok(module_context.functions[&id].into())
            }
            Expression::Block(block) => {
                let mut value = None;
                for statement in &block.statements {
                    value =
                        self.compile_statement(llvm_module, module_context, module, statement)?;
                }
                Ok(value.unwrap_or_else(|| {
                    self.context
                        .struct_type(&[], false)
                        .const_zero()
                        .as_any_value_enum()
                }))
            }
            Expression::List(list) => self
                .compile_list_expression(llvm_module, module_context, module, list)
                .map(AnyValueEnum::from),
            Expression::Call(call) => {
                let callee =
                    self.compile_expression(llvm_module, module_context, module, &call.callee)?;
                let arguments =
                    self.compile_arguments(llvm_module, module_context, module, &call.argument)?;
                let AnyValueEnum::FunctionValue(function) = callee else {
                    return Err(Diagnostic::new(
                        call.callee.syntax().span.clone(),
                        "expression is not directly callable",
                    ));
                };
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
                let value =
                    self.compile_expression(llvm_module, module_context, module, &access.value)?;
                let Some(BasicValueEnum::StructValue(value)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        access.value.syntax().span.clone(),
                        "element access requires a list value",
                    ));
                };
                let index = match &access.accessor {
                    Accessor::Index(index) => index.parse::<u32>().map_err(|_| {
                        Diagnostic::new(access.syntax.span.clone(), "invalid list index")
                    })?,
                    Accessor::Name(name) => {
                        list_element_index(&access.value, name).ok_or_else(|| {
                            Diagnostic::new(
                                access.syntax.span.clone(),
                                format!("cannot determine index of list element `{name}`"),
                            )
                        })?
                    }
                };
                self.builder
                    .build_extract_value(value, index, "element")
                    .map(|value| value.as_any_value_enum())
                    .map_err(|error| Diagnostic::new(access.syntax.span.clone(), error.to_string()))
            }
            Expression::Binary(binary) => {
                self.compile_binary_expression(llvm_module, module_context, module, binary)
            }
            Expression::Name(name) => {
                let symbol = module
                    .symbol_for(name.syntax.id)
                    .ok_or_else(|| Diagnostic::new(name.syntax.span.clone(), "unresolved name"))?;
                module_context.values.get(&symbol).copied().ok_or_else(|| {
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
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
        binary: &BinaryExpression,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let AnyValueEnum::IntValue(left) =
            self.compile_expression(llvm_module, module_context, module, &binary.left)?
        else {
            return Err(Diagnostic::new(
                binary.left.syntax().span.clone(),
                "arithmetic operands must be integers",
            ));
        };
        let AnyValueEnum::IntValue(right) =
            self.compile_expression(llvm_module, module_context, module, &binary.right)?
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

    fn compile_list_expression(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
        list: &ListExpression,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
        let values = list
            .elements
            .iter()
            .map(|element| {
                self.compile_expression(llvm_module, module_context, module, &element.value)
                    .and_then(|value| {
                        value_as_basic(value).ok_or_else(|| {
                            Diagnostic::new(
                                element.syntax.span.clone(),
                                "list element is not a first-class value",
                            )
                        })
                    })
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let types = values
            .iter()
            .map(BasicValueEnum::get_type)
            .collect::<Vec<_>>();
        let mut list_value = self.context.struct_type(&types, true).const_zero();
        for (index, element) in values.into_iter().enumerate() {
            list_value = self
                .builder
                .build_insert_value(list_value, element, index as u32, "list.element")
                .map_err(|error| Diagnostic::new(list.syntax.span.clone(), error.to_string()))?
                .into_struct_value();
        }
        Ok(list_value)
    }

    fn compile_arguments(
        &self,
        llvm_module: &inkwell::module::Module<'context>,
        module_context: &mut ModuleContext<'context>,
        module: &TypedModule,
        argument: &Expression,
    ) -> CodeGenerationResult<Vec<inkwell::values::BasicMetadataValueEnum<'context>>> {
        let expressions: Vec<&Expression> = match argument {
            Expression::List(list) => list.elements.iter().map(|element| &element.value).collect(),
            expression => vec![expression],
        };
        expressions
            .into_iter()
            .map(|expression| {
                self.compile_expression(llvm_module, module_context, module, expression)
                    .and_then(|value| {
                        value_as_basic(value).map(Into::into).ok_or_else(|| {
                            Diagnostic::new(
                                expression.syntax().span.clone(),
                                "argument is not a first-class value",
                            )
                        })
                    })
            })
            .collect()
    }

    fn compile_function_type(
        &self,
        function_type: &CheckedFunctionType,
    ) -> CodeGenerationResult<inkwell::types::FunctionType<'context>> {
        let return_type = self.compile_type(&function_type.result)?;
        let parameter_types = self.compile_parameter_types(&function_type.parameter)?;
        let variadic = matches!(
            &*function_type.parameter,
            CheckedType::List(list) if list.variadic
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
            CheckedType::List(list) => list
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
            CheckedType::List(list) => self.compile_list_type(list).map(Into::into),
            CheckedType::I32 => Ok(self.context.i32_type().into()),
            CheckedType::Bool => Ok(self.context.bool_type().into()),
            CheckedType::Distinct { representation, .. } => self.compile_type(representation),
        }
    }

    fn compile_list_type(
        &self,
        list: &CheckedListType,
    ) -> CodeGenerationResult<inkwell::types::StructType<'context>> {
        let fields = list
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

fn list_element_index(expression: &Expression, name: &str) -> Option<u32> {
    let Expression::List(list) = expression else {
        return None;
    };
    list.elements
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
