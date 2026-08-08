use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::{
    Accessor, Binding, Diagnostic, Expression, FunctionId, Item, Module, Pattern, PrimitiveType,
    ProductType, ResolvedFunction, ResolvedModule, Span, Statement, SymbolId, SyntaxId, Type,
    TypeDeclaration, TypeDeclarationKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckedType {
    Inferred,
    Error,
    I32,
    Bool,
    String,
    CChar,
    Pointer {
        is_const: bool,
        pointee: Box<CheckedType>,
    },
    Product(CheckedProductType),
    Function(CheckedFunctionType),
    Distinct {
        name: String,
        representation: Box<CheckedType>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedProductType {
    pub elements: Vec<CheckedTypeElement>,
    pub variadic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTypeElement {
    pub name: Option<String>,
    pub value_type: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedFunctionType {
    pub parameter: Box<CheckedType>,
    pub result: Box<CheckedType>,
}

impl CheckedType {
    pub fn empty_product() -> Self {
        Self::Product(CheckedProductType {
            elements: Vec::new(),
            variadic: false,
        })
    }

    pub fn is_concrete(&self) -> bool {
        match self {
            Self::Inferred | Self::Error => false,
            Self::Pointer { pointee, .. } => pointee.is_concrete(),
            Self::Product(product) => product
                .elements
                .iter()
                .all(|element| element.value_type.is_concrete()),
            Self::Function(function) => {
                function.parameter.is_concrete() && function.result.is_concrete()
            }
            Self::Distinct { representation, .. } => representation.is_concrete(),
            Self::I32 | Self::Bool | Self::String | Self::CChar => true,
        }
    }
}

impl fmt::Display for CheckedType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inferred => formatter.write_str("_"),
            Self::Error => formatter.write_str("<error>"),
            Self::I32 => formatter.write_str("i32"),
            Self::Bool => formatter.write_str("bool"),
            Self::String => formatter.write_str("string"),
            Self::CChar => formatter.write_str("c_char"),
            Self::Pointer { is_const, pointee } => {
                formatter.write_str("*")?;
                if *is_const {
                    formatter.write_str("const ")?;
                }
                write!(formatter, "{pointee}")
            }
            Self::Product(product) => {
                formatter.write_str("(")?;
                for (index, element) in product.elements.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    if let Some(name) = &element.name {
                        write!(formatter, "{name}: ")?;
                    }
                    write!(formatter, "{}", element.value_type)?;
                }
                if product.variadic {
                    if !product.elements.is_empty() {
                        formatter.write_str(", ")?;
                    }
                    formatter.write_str("...")?;
                }
                formatter.write_str(")")
            }
            Self::Function(function) => {
                write!(formatter, "{} -> {}", function.parameter, function.result)
            }
            Self::Distinct { name, .. } => formatter.write_str(name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TypedModule {
    resolved: ResolvedModule,
    expression_types: HashMap<SyntaxId, CheckedType>,
    symbol_types: HashMap<SymbolId, CheckedType>,
    function_types: HashMap<FunctionId, CheckedFunctionType>,
}

impl TypedModule {
    pub fn resolved(&self) -> &ResolvedModule {
        &self.resolved
    }

    pub fn syntax(&self) -> &Module {
        self.resolved.syntax()
    }

    pub fn functions(&self) -> &[ResolvedFunction] {
        self.resolved.functions()
    }

    pub fn symbol_for(&self, syntax_id: SyntaxId) -> Option<SymbolId> {
        self.resolved.symbol_for(syntax_id)
    }

    pub fn function_for(&self, syntax_id: SyntaxId) -> Option<FunctionId> {
        self.resolved.function_for(syntax_id)
    }

    pub fn type_of_expression(&self, syntax_id: SyntaxId) -> Option<&CheckedType> {
        self.expression_types.get(&syntax_id)
    }

    pub fn type_of_symbol(&self, symbol: SymbolId) -> Option<&CheckedType> {
        self.symbol_types.get(&symbol)
    }

    pub fn type_of_function(&self, function: FunctionId) -> Option<&CheckedFunctionType> {
        self.function_types.get(&function)
    }
}

#[derive(Default)]
pub struct TypeChecker {
    expression_types: HashMap<SyntaxId, CheckedType>,
    symbol_types: HashMap<SymbolId, CheckedType>,
    function_types: HashMap<FunctionId, CheckedFunctionType>,
    function_symbols: HashMap<SymbolId, FunctionId>,
    checking_functions: HashSet<FunctionId>,
    checked_functions: HashSet<FunctionId>,
    type_declarations: HashMap<String, TypeDeclaration>,
    resolved_named_types: HashMap<String, CheckedType>,
    resolving_named_types: HashSet<String>,
    diagnostics: Vec<Diagnostic>,
}

impl TypeChecker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(mut self, module: ResolvedModule) -> Result<TypedModule, Vec<Diagnostic>> {
        self.collect_type_declarations(module.syntax());
        self.seed_declared_bindings(&module);
        self.seed_function_types(&module);

        let function_ids = module
            .functions()
            .iter()
            .map(|function| function.id)
            .collect::<Vec<_>>();
        for function_id in function_ids {
            self.ensure_function_checked(&module, function_id);
        }
        for item in &module.syntax().items {
            self.check_item(&module, item);
        }

        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        Ok(TypedModule {
            resolved: module,
            expression_types: self.expression_types,
            symbol_types: self.symbol_types,
            function_types: self.function_types,
        })
    }

    fn collect_type_declarations(&mut self, module: &Module) {
        for item in &module.items {
            if let Item::TypeDeclaration(declaration) = item
                && self
                    .type_declarations
                    .insert(declaration.name.clone(), declaration.clone())
                    .is_some()
            {
                self.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    format!("duplicate type definition of `{}`", declaration.name),
                ));
            }
        }
    }

    fn seed_declared_bindings(&mut self, module: &ResolvedModule) {
        for item in &module.syntax().items {
            match item {
                Item::ExternBlock(block) => {
                    for binding in &block.bindings {
                        self.seed_binding_annotation(module, binding);
                    }
                }
                Item::Statement(statement) => {
                    if let Statement::Binding(binding) = statement.as_ref() {
                        self.seed_binding_annotation(module, binding);
                    }
                }
                Item::TypeDeclaration(_) => {}
            }
        }
    }

    fn seed_binding_annotation(&mut self, module: &ResolvedModule, binding: &Binding) {
        let Some(annotation) = &binding.annotation else {
            return;
        };
        let value_type = self.resolve_source_type(annotation);
        if let Some(symbol) = module.symbol_for(binding.syntax.id) {
            self.symbol_types.insert(symbol, value_type);
        }
    }

    fn seed_function_types(&mut self, module: &ResolvedModule) {
        for function in module.functions() {
            let parameter = self.resolve_source_type(&function.pattern.ty());
            let result = function
                .return_annotation
                .as_ref()
                .map(|value_type| self.resolve_source_type(value_type))
                .or_else(|| {
                    function.binding_annotation.as_ref().and_then(|annotation| {
                        let CheckedType::Function(function_type) =
                            self.resolve_source_type(annotation)
                        else {
                            return None;
                        };
                        Some(*function_type.result)
                    })
                })
                .unwrap_or(CheckedType::Inferred);
            let mut function_type = CheckedFunctionType {
                parameter: Box::new(parameter),
                result: Box::new(result),
            };

            if let Some(annotation) = &function.binding_annotation {
                let binding_type = self.resolve_source_type(annotation);
                let actual = CheckedType::Function(function_type.clone());
                let merged =
                    self.require_compatible(actual, binding_type, annotation.syntax().span.clone());
                if let CheckedType::Function(merged_function) = merged {
                    function_type = merged_function;
                }
            }

            if let Some(binding_syntax) = function.binding_syntax
                && let Some(symbol) = module.symbol_for(binding_syntax)
            {
                self.function_symbols.insert(symbol, function.id);
                self.symbol_types
                    .insert(symbol, CheckedType::Function(function_type.clone()));
            }
            self.function_types.insert(function.id, function_type);
        }
    }

    fn ensure_function_checked(&mut self, module: &ResolvedModule, function_id: FunctionId) {
        if self.checked_functions.contains(&function_id)
            || !self.checking_functions.insert(function_id)
        {
            return;
        }
        let function = module
            .functions()
            .iter()
            .find(|function| function.id == function_id)
            .cloned()
            .expect("resolved function ID must be valid");
        let function_type = self.function_types[&function.id].clone();
        self.bind_pattern_types(module, &function.pattern, &function_type.parameter);
        let body_type = self.check_expression(module, &function.body);
        let result_type = self.require_compatible(
            body_type,
            (*function_type.result).clone(),
            function.body.syntax().span.clone(),
        );
        let checked_function_type = CheckedFunctionType {
            parameter: function_type.parameter,
            result: Box::new(result_type),
        };
        self.function_types
            .insert(function.id, checked_function_type.clone());
        if let Some(binding_syntax) = function.binding_syntax
            && let Some(symbol) = module.symbol_for(binding_syntax)
        {
            self.symbol_types
                .insert(symbol, CheckedType::Function(checked_function_type));
        }
        self.checking_functions.remove(&function_id);
        self.checked_functions.insert(function_id);
    }

    fn bind_pattern_types(
        &mut self,
        module: &ResolvedModule,
        pattern: &Pattern,
        value_type: &CheckedType,
    ) {
        match pattern {
            Pattern::Binding(binding) => {
                if let Some(symbol) = module.symbol_for(binding.syntax.id) {
                    self.symbol_types.insert(symbol, value_type.clone());
                }
            }
            Pattern::Product(product) if product.elements.len() == 1 => {
                self.bind_pattern_types(module, &product.elements[0], value_type);
            }
            Pattern::Product(product) => {
                let CheckedType::Product(product_type) = value_type else {
                    return;
                };
                for (pattern, element) in product.elements.iter().zip(&product_type.elements) {
                    self.bind_pattern_types(module, pattern, &element.value_type);
                }
            }
        }
    }

    fn check_item(&mut self, module: &ResolvedModule, item: &Item) {
        match item {
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    self.check_binding(module, binding);
                }
            }
            Item::Statement(statement) => {
                self.check_statement(module, statement);
            }
            Item::TypeDeclaration(_) => {}
        }
    }

    fn check_statement(&mut self, module: &ResolvedModule, statement: &Statement) -> CheckedType {
        match statement {
            Statement::Binding(binding) => {
                self.check_binding(module, binding);
                CheckedType::empty_product()
            }
            Statement::Expression(expression) => self.check_expression(module, expression),
        }
    }

    fn check_binding(&mut self, module: &ResolvedModule, binding: &Binding) {
        let symbol = module.symbol_for(binding.syntax.id);
        let declared_type = binding
            .annotation
            .as_ref()
            .map(|annotation| self.resolve_source_type(annotation));
        let value_type = binding
            .value
            .as_ref()
            .map(|value| self.check_expression(module, value));

        let checked_type = match (value_type, declared_type) {
            (Some(actual), Some(expected)) => {
                self.require_compatible(actual, expected, binding.syntax.span.clone())
            }
            (Some(actual), None) => actual,
            (None, Some(expected)) => expected,
            (None, None) => {
                self.diagnostics.push(Diagnostic::new(
                    binding.syntax.span.clone(),
                    format!(
                        "cannot infer the type of `{}` without a value",
                        binding.name
                    ),
                ));
                CheckedType::Error
            }
        };
        if !checked_type.is_concrete() && checked_type != CheckedType::Error {
            self.diagnostics.push(Diagnostic::new(
                binding.syntax.span.clone(),
                format!("could not fully infer the type of `{}`", binding.name),
            ));
        }
        if let Some(symbol) = symbol {
            self.symbol_types.insert(symbol, checked_type);
        }
    }

    fn check_expression(
        &mut self,
        module: &ResolvedModule,
        expression: &Expression,
    ) -> CheckedType {
        let value_type = match expression {
            Expression::Function(function) => {
                let Some(function_id) = module.function_for(function.syntax.id) else {
                    return CheckedType::Error;
                };
                self.ensure_function_checked(module, function_id);
                self.function_types
                    .get(&function_id)
                    .cloned()
                    .map(CheckedType::Function)
                    .unwrap_or(CheckedType::Error)
            }
            Expression::Block(block) => {
                let mut result = CheckedType::empty_product();
                for statement in &block.statements {
                    result = self.check_statement(module, statement);
                }
                result
            }
            Expression::Product(product) => normalize_product_type(
                product
                    .elements
                    .iter()
                    .map(|element| CheckedTypeElement {
                        name: element.name.clone(),
                        value_type: self.check_expression(module, &element.value),
                    })
                    .collect(),
                false,
            ),
            Expression::Call(call) => {
                let callee_type = self.check_expression(module, &call.callee);
                let argument_type = self.check_expression(module, &call.argument);
                match callee_type {
                    CheckedType::Function(function) => {
                        self.check_call_argument(
                            argument_type,
                            &function.parameter,
                            call.argument.syntax().span.clone(),
                        );
                        *function.result
                    }
                    CheckedType::Error => CheckedType::Error,
                    other => {
                        self.diagnostics.push(Diagnostic::new(
                            call.callee.syntax().span.clone(),
                            format!("cannot call a value of type `{other}`"),
                        ));
                        CheckedType::Error
                    }
                }
            }
            Expression::Access(access) => {
                let value_type = self.check_expression(module, &access.value);
                match value_type {
                    CheckedType::Product(product) => match &access.accessor {
                        Accessor::Index(index) => index
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| product.elements.get(index))
                            .map(|element| element.value_type.clone())
                            .unwrap_or_else(|| {
                                self.diagnostics.push(Diagnostic::new(
                                    access.syntax.span.clone(),
                                    format!("product index `{index}` is out of bounds"),
                                ));
                                CheckedType::Error
                            }),
                        Accessor::Name(name) => product
                            .elements
                            .iter()
                            .find(|element| element.name.as_deref() == Some(name))
                            .map(|element| element.value_type.clone())
                            .unwrap_or_else(|| {
                                self.diagnostics.push(Diagnostic::new(
                                    access.syntax.span.clone(),
                                    format!("product has no element named `{name}`"),
                                ));
                                CheckedType::Error
                            }),
                    },
                    CheckedType::Error => CheckedType::Error,
                    other => {
                        self.diagnostics.push(Diagnostic::new(
                            access.value.syntax().span.clone(),
                            format!("cannot access an element of `{other}`"),
                        ));
                        CheckedType::Error
                    }
                }
            }
            Expression::Binary(binary) => {
                let left = self.check_expression(module, &binary.left);
                let right = self.check_expression(module, &binary.right);
                self.require_compatible(left, CheckedType::I32, binary.left.syntax().span.clone());
                self.require_compatible(
                    right,
                    CheckedType::I32,
                    binary.right.syntax().span.clone(),
                );
                CheckedType::I32
            }
            Expression::Name(name) => {
                let symbol = module.symbol_for(name.syntax.id);
                if let Some(function_id) =
                    symbol.and_then(|symbol| self.function_symbols.get(&symbol).copied())
                {
                    self.ensure_function_checked(module, function_id);
                }
                symbol
                    .and_then(|symbol| self.symbol_types.get(&symbol).cloned())
                    .unwrap_or_else(|| {
                        self.diagnostics.push(Diagnostic::new(
                            name.syntax.span.clone(),
                            format!("the type of `{}` is not available here", name.name),
                        ));
                        CheckedType::Error
                    })
            }
            Expression::String(_) => CheckedType::String,
            Expression::Integer(_) => CheckedType::I32,
        };
        self.expression_types
            .insert(expression.syntax().id, value_type.clone());
        value_type
    }

    fn check_call_argument(&mut self, actual: CheckedType, expected: &CheckedType, span: Span) {
        if let CheckedType::Product(expected_product) = expected
            && expected_product.variadic
        {
            let actual_elements = match actual {
                CheckedType::Product(product) => product.elements,
                value_type => vec![CheckedTypeElement {
                    name: None,
                    value_type,
                }],
            };
            if actual_elements.len() < expected_product.elements.len() {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "expected at least {} arguments",
                        expected_product.elements.len()
                    ),
                ));
                return;
            }
            for (actual, expected) in actual_elements.into_iter().zip(&expected_product.elements) {
                self.require_compatible(
                    actual.value_type,
                    expected.value_type.clone(),
                    span.clone(),
                );
            }
            return;
        }
        self.require_compatible(actual, expected.clone(), span);
    }

    fn require_compatible(
        &mut self,
        actual: CheckedType,
        expected: CheckedType,
        span: Span,
    ) -> CheckedType {
        match merge_types(actual.clone(), expected.clone()) {
            Some(value_type) => value_type,
            None => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("expected `{expected}`, found `{actual}`"),
                ));
                CheckedType::Error
            }
        }
    }

    fn resolve_source_type(&mut self, source_type: &Type) -> CheckedType {
        match source_type {
            Type::Inferred(_) => CheckedType::Inferred,
            Type::Named(named) => self.resolve_named_type(&named.name, named.syntax.span.clone()),
            Type::Pointer(pointer) => CheckedType::Pointer {
                is_const: pointer.is_const,
                pointee: Box::new(self.resolve_source_type(&pointer.pointee)),
            },
            Type::Product(product) => {
                let product = self.resolve_product_type(product);
                normalize_product_type(product.elements, product.variadic)
            }
            Type::Function(function) => CheckedType::Function(CheckedFunctionType {
                parameter: Box::new(self.resolve_source_type(&function.parameter)),
                result: Box::new(self.resolve_source_type(&function.result)),
            }),
            Type::Primitive(PrimitiveType::I32(_)) => CheckedType::I32,
            Type::Primitive(PrimitiveType::Bool(_)) => CheckedType::Bool,
        }
    }

    fn resolve_product_type(&mut self, product: &ProductType) -> CheckedProductType {
        CheckedProductType {
            elements: product
                .elements
                .iter()
                .map(|element| CheckedTypeElement {
                    name: element.name.clone(),
                    value_type: self.resolve_source_type(&element.ty),
                })
                .collect(),
            variadic: product.variadic,
        }
    }

    fn resolve_named_type(&mut self, name: &str, span: Span) -> CheckedType {
        match name {
            "int" => return CheckedType::I32,
            "string" => return CheckedType::String,
            "c_char" => return CheckedType::CChar,
            _ => {}
        }
        if let Some(value_type) = self.resolved_named_types.get(name) {
            return value_type.clone();
        }
        let Some(declaration) = self.type_declarations.get(name).cloned() else {
            self.diagnostics
                .push(Diagnostic::new(span, format!("unknown type `{name}`")));
            return CheckedType::Error;
        };
        if !self.resolving_named_types.insert(name.to_owned()) {
            self.diagnostics.push(Diagnostic::new(
                declaration.syntax.span.clone(),
                format!("cyclic type definition involving `{name}`"),
            ));
            return CheckedType::Error;
        }
        let representation = self.resolve_source_type(&declaration.underlying);
        self.resolving_named_types.remove(name);
        let value_type = match declaration.kind {
            TypeDeclarationKind::Alias => representation,
            TypeDeclarationKind::Distinct => CheckedType::Distinct {
                name: name.to_owned(),
                representation: Box::new(representation),
            },
        };
        self.resolved_named_types
            .insert(name.to_owned(), value_type.clone());
        value_type
    }
}

fn merge_types(actual: CheckedType, expected: CheckedType) -> Option<CheckedType> {
    match (actual, expected) {
        (CheckedType::Error, _) | (_, CheckedType::Error) => Some(CheckedType::Error),
        (CheckedType::Inferred, value_type) | (value_type, CheckedType::Inferred) => {
            Some(value_type)
        }
        (CheckedType::I32, CheckedType::I32) => Some(CheckedType::I32),
        (CheckedType::Bool, CheckedType::Bool) => Some(CheckedType::Bool),
        (CheckedType::String, CheckedType::String) => Some(CheckedType::String),
        (CheckedType::CChar, CheckedType::CChar) => Some(CheckedType::CChar),
        (
            CheckedType::String,
            CheckedType::Pointer {
                is_const: true,
                pointee,
            },
        ) if *pointee == CheckedType::CChar => Some(CheckedType::String),
        (
            CheckedType::Pointer {
                is_const: actual_const,
                pointee: actual_pointee,
            },
            CheckedType::Pointer {
                is_const: expected_const,
                pointee: expected_pointee,
            },
        ) if actual_const == expected_const => {
            merge_types(*actual_pointee, *expected_pointee).map(|pointee| CheckedType::Pointer {
                is_const: actual_const,
                pointee: Box::new(pointee),
            })
        }
        (CheckedType::Product(actual), CheckedType::Product(expected))
            if actual.variadic == expected.variadic
                && actual.elements.len() == expected.elements.len() =>
        {
            let elements = actual
                .elements
                .into_iter()
                .zip(expected.elements)
                .map(|(actual, expected)| {
                    merge_types(actual.value_type, expected.value_type).map(|value_type| {
                        CheckedTypeElement {
                            name: expected.name.or(actual.name),
                            value_type,
                        }
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(normalize_product_type(elements, actual.variadic))
        }
        (CheckedType::Function(actual), CheckedType::Function(expected)) => {
            Some(CheckedType::Function(CheckedFunctionType {
                parameter: Box::new(merge_types(*actual.parameter, *expected.parameter)?),
                result: Box::new(merge_types(*actual.result, *expected.result)?),
            }))
        }
        (
            CheckedType::Distinct {
                name: actual_name,
                representation: actual_representation,
            },
            CheckedType::Distinct {
                name: expected_name,
                representation: expected_representation,
            },
        ) if actual_name == expected_name => Some(CheckedType::Distinct {
            name: actual_name,
            representation: Box::new(merge_types(
                *actual_representation,
                *expected_representation,
            )?),
        }),
        _ => None,
    }
}

fn normalize_product_type(elements: Vec<CheckedTypeElement>, variadic: bool) -> CheckedType {
    if !variadic && elements.len() == 1 {
        elements.into_iter().next().unwrap().value_type
    } else {
        CheckedType::Product(CheckedProductType { elements, variadic })
    }
}
