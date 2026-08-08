use std::collections::HashMap;

use crate::{
    Accessor, Binding, BindingKind, Diagnostic, Expression, Item, MacroDeclaration, ModuleId,
    Pattern, Program, Span, Statement, Syntax, SyntaxId, Type, UseKind, Visibility,
};

const MAX_EXPANSION_DEPTH: usize = 128;
const MAX_EVALUATION_STEPS: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MacroKey {
    module: ModuleId,
    name: String,
}

#[derive(Clone)]
enum MacroKind {
    User(Expression),
    CString,
    Quote,
}

#[derive(Clone)]
struct MacroDefinition {
    key: MacroKey,
    declaration: MacroDeclaration,
    arity: usize,
    kind: MacroKind,
}

#[derive(Clone, Default)]
struct ModuleScope {
    macros: HashMap<String, MacroKey>,
    namespaces: HashMap<String, ModuleId>,
    helpers: HashMap<String, HelperDefinition>,
}

#[derive(Clone)]
struct HelperDefinition {
    module: ModuleId,
    binding: Binding,
}

#[derive(Clone)]
enum Value {
    Syntax(Expression),
    Function {
        module: ModuleId,
        function: crate::FunctionExpression,
        environment: Environment,
    },
    Helper(ModuleId, Binding),
    Product(Vec<(Option<String>, Value)>),
    Integer(i128),
    String(String),
    Nominal(String, Box<Value>),
}

type Environment = HashMap<String, Value>;

pub(crate) fn expand_program(mut program: Program) -> Result<Program, Vec<Diagnostic>> {
    let mut expander = MacroExpander::new(&program);
    expander.validate_definitions();
    for module in program.modules() {
        for item in &module.syntax.items {
            if let Item::Statement(statement) = item
                && let Statement::Binding(binding) = statement.as_ref()
                && binding.kind != BindingKind::Def
                && binding_contains_syntax(binding)
            {
                expander.diagnostics.push(Diagnostic::new(
                    binding.syntax.span.clone(),
                    "`Syntax` values are compile-time-only",
                ));
            }
        }
    }
    if !expander.diagnostics.is_empty() {
        return Err(expander.diagnostics);
    }

    for source_module in program.modules_mut() {
        let module = source_module.id;
        let mut items = source_module.syntax.items.clone();
        for item in &mut items {
            expander.expand_item(module, item);
        }
        items.retain(|item| {
            !matches!(item,
                Item::Statement(statement)
                    if matches!(statement.as_ref(), Statement::Binding(binding)
                        if binding_is_compile_time_helper(binding))
            )
        });
        source_module.syntax.items = items;
    }
    if expander.diagnostics.is_empty() {
        Ok(program)
    } else {
        Err(expander.diagnostics)
    }
}

struct MacroExpander {
    definitions: HashMap<MacroKey, MacroDefinition>,
    scopes: Vec<ModuleScope>,
    diagnostics: Vec<Diagnostic>,
    next_syntax_id: usize,
    next_mark: u64,
    steps: usize,
    expansion_stack: Vec<MacroKey>,
}

impl MacroExpander {
    fn new(program: &Program) -> Self {
        let mut definitions = HashMap::new();
        let mut scopes = vec![ModuleScope::default(); program.modules().len()];
        let core = program.standard_library_core();
        let cinterop = program.standard_library_cinterop();

        for source_module in program.modules() {
            for item in &source_module.syntax.items {
                match item {
                    Item::MacroDeclaration(declaration) => {
                        let key = MacroKey {
                            module: source_module.id,
                            name: declaration.name.clone(),
                        };
                        let kind = if Some(source_module.id) == core && declaration.name == "quote"
                        {
                            MacroKind::Quote
                        } else if Some(source_module.id) == cinterop
                            && declaration.name == "c_string"
                        {
                            MacroKind::CString
                        } else if let Some(value) = &declaration.value {
                            MacroKind::User(value.clone())
                        } else {
                            MacroKind::User(Expression::Product(crate::ProductExpression::empty()))
                        };
                        let arity = declaration
                            .annotation
                            .as_ref()
                            .map(macro_annotation_arity)
                            .unwrap_or_else(|| {
                                declaration
                                    .value
                                    .as_ref()
                                    .map(expression_arity)
                                    .unwrap_or(0)
                            });
                        definitions.insert(
                            key.clone(),
                            MacroDefinition {
                                key: key.clone(),
                                declaration: declaration.clone(),
                                arity,
                                kind,
                            },
                        );
                        scopes[source_module.id.0]
                            .macros
                            .insert(declaration.name.clone(), key);
                    }
                    Item::Statement(statement) => {
                        if let Statement::Binding(binding) = statement.as_ref()
                            && binding.value.is_some()
                        {
                            scopes[source_module.id.0].helpers.insert(
                                binding.name.clone(),
                                HelperDefinition {
                                    module: source_module.id,
                                    binding: binding.clone(),
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        let public_macros = definitions
            .values()
            .filter(|definition| definition.declaration.visibility == Visibility::Public)
            .map(|definition| (definition.key.clone(), definition.key.clone()))
            .collect::<HashMap<_, _>>();
        let public_helpers = program
            .modules()
            .iter()
            .flat_map(|module| {
                scopes[module.id.0]
                    .helpers
                    .iter()
                    .filter(|(_, helper)| helper.binding.visibility == Visibility::Public)
                    .map(move |(name, helper)| ((module.id, name.clone()), helper.clone()))
            })
            .collect::<HashMap<_, _>>();

        for source_module in program.modules() {
            if let Some(core) = core
                && source_module.id != core
            {
                for definition in definitions.values().filter(|definition| {
                    definition.key.module == core
                        && definition.declaration.visibility == Visibility::Public
                }) {
                    scopes[source_module.id.0]
                        .macros
                        .entry(definition.key.name.clone())
                        .or_insert_with(|| definition.key.clone());
                }
                for ((module, name), binding) in &public_helpers {
                    if *module == core {
                        scopes[source_module.id.0]
                            .helpers
                            .entry(name.clone())
                            .or_insert_with(|| binding.clone());
                    }
                }
            }
            for item in &source_module.syntax.items {
                let Item::UseDeclaration(use_) = item else {
                    continue;
                };
                let Some(imported) = program.imported_module(use_.syntax.id) else {
                    continue;
                };
                match &use_.kind {
                    UseKind::Namespace => {
                        if let Some(name) = use_.path.last() {
                            scopes[source_module.id.0]
                                .namespaces
                                .insert(name.clone(), imported);
                        }
                    }
                    UseKind::Glob => {
                        for (key, _) in public_macros
                            .iter()
                            .filter(|(key, _)| key.module == imported)
                        {
                            scopes[source_module.id.0]
                                .macros
                                .entry(key.name.clone())
                                .or_insert_with(|| key.clone());
                        }
                        for ((module, name), binding) in &public_helpers {
                            if *module == imported {
                                scopes[source_module.id.0]
                                    .helpers
                                    .entry(name.clone())
                                    .or_insert_with(|| binding.clone());
                            }
                        }
                    }
                    UseKind::Selected(names) => {
                        for name in names {
                            Self::install_selected(
                                &mut scopes[source_module.id.0],
                                imported,
                                name,
                                name,
                                &public_macros,
                                &public_helpers,
                            );
                        }
                    }
                    UseKind::Renamed { item, alias } => Self::install_selected(
                        &mut scopes[source_module.id.0],
                        imported,
                        item,
                        alias,
                        &public_macros,
                        &public_helpers,
                    ),
                }
            }
        }

        let next_syntax_id = program
            .modules()
            .iter()
            .map(|module| module.syntax.syntax.id.0)
            .max()
            .unwrap_or(0)
            + 1;
        Self {
            definitions,
            scopes,
            diagnostics: Vec::new(),
            next_syntax_id,
            next_mark: 1,
            steps: 0,
            expansion_stack: Vec::new(),
        }
    }

    fn install_selected(
        scope: &mut ModuleScope,
        imported: ModuleId,
        item: &str,
        local: &str,
        public_macros: &HashMap<MacroKey, MacroKey>,
        public_helpers: &HashMap<(ModuleId, String), HelperDefinition>,
    ) {
        let key = MacroKey {
            module: imported,
            name: item.to_owned(),
        };
        if public_macros.contains_key(&key) {
            scope.macros.entry(local.to_owned()).or_insert(key);
        }
        if let Some(helper) = public_helpers.get(&(imported, item.to_owned())) {
            scope
                .helpers
                .entry(local.to_owned())
                .or_insert_with(|| helper.clone());
        }
    }

    fn validate_definitions(&mut self) {
        for definition in self.definitions.values() {
            if let Some(annotation) = &definition.declaration.annotation
                && !valid_macro_annotation(annotation)
            {
                self.diagnostics.push(Diagnostic::new(
                    annotation.syntax().span.clone(),
                    "a macro annotation must be one or more `Syntax` parameters returning `Syntax`",
                ));
            }
            match &definition.kind {
                MacroKind::CString | MacroKind::Quote
                    if definition.arity != 1
                        || definition
                            .declaration
                            .annotation
                            .as_ref()
                            .is_none_or(|annotation| !valid_macro_annotation(annotation)) =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "compiler-provided macro `{}` must have signature `Syntax -> Syntax`",
                            definition.key.name
                        ),
                    ));
                }
                MacroKind::User(_) if definition.declaration.value.is_none() => {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "macro `{}` requires a body; only compiler-provided macros may be bodyless",
                            definition.key.name
                        ),
                    ));
                }
                MacroKind::User(_) if definition.arity == 0 => {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "macro `{}` must accept at least one parameter",
                            definition.key.name
                        ),
                    ))
                }
                MacroKind::User(body) if obviously_not_syntax(body, definition.arity) => {
                    self.diagnostics.push(Diagnostic::new(
                        body.syntax().span.clone(),
                        format!("macro `{}` must return `Syntax`", definition.key.name),
                    ));
                }
                MacroKind::User(body)
                    if !valid_macro_parameter_patterns(body, definition.arity) =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        body.syntax().span.clone(),
                        format!(
                            "macro `{}` parameters must bind `Syntax` values",
                            definition.key.name
                        ),
                    ));
                }
                _ => {}
            }
        }
    }

    fn expand_item(&mut self, module: ModuleId, item: &mut Item) {
        match item {
            Item::Statement(statement) => self.expand_statement(module, statement, 0),
            Item::TraitImplementation(implementation) => {
                for member in &mut implementation.members {
                    member.value = self.expand_expression(module, member.value.clone(), 0);
                }
            }
            Item::MacroDeclaration(_)
            | Item::UseDeclaration(_)
            | Item::ExternBlock(_)
            | Item::TypeDeclaration(_)
            | Item::TraitDeclaration(_) => {}
        }
    }

    fn expand_statement(&mut self, module: ModuleId, statement: &mut Statement, depth: usize) {
        match statement {
            Statement::Binding(binding) => {
                if let Some(value) = binding.value.take() {
                    binding.value = Some(self.expand_expression(module, value, depth));
                }
            }
            Statement::PatternBinding(binding) => {
                binding.value = self.expand_expression(module, binding.value.clone(), depth);
            }
            Statement::Return(return_) => {
                return_.value = self.expand_expression(module, return_.value.clone(), depth);
            }
            Statement::Expression(expression) => {
                *expression = self.expand_expression(module, expression.clone(), depth);
            }
        }
    }

    fn expand_expression(
        &mut self,
        module: ModuleId,
        expression: Expression,
        depth: usize,
    ) -> Expression {
        if depth >= MAX_EXPANSION_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                expression.syntax().span.clone(),
                "macro expansion exceeded the limit of 128 nested expansions",
            ));
            return expression;
        }
        let (head, arguments) = flatten_call(&expression);
        if let Some(key) = self.resolve_macro(module, head) {
            let Some(definition) = self.definitions.get(&key).cloned() else {
                return expression;
            };
            if arguments.len() < definition.arity {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    format!(
                        "macro `{}` requires {} argument{} but received {}",
                        key.name,
                        definition.arity,
                        if definition.arity == 1 { "" } else { "s" },
                        arguments.len()
                    ),
                ));
                return expression;
            }
            if self.expansion_stack.contains(&key) {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    format!("recursive macro expansion of `{}`", key.name),
                ));
                return expression;
            }
            if depth == 0 {
                self.steps = 0;
            }
            self.expansion_stack.push(key.clone());
            let consumed = arguments[..definition.arity].to_vec();
            let diagnostic_start = self.diagnostics.len();
            let expanded =
                self.invoke_macro(&definition, consumed, expression.syntax().span.clone());
            let Some(mut result) = expanded else {
                self.expansion_stack.pop();
                if self.diagnostics.len() > diagnostic_start {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!("while expanding macro `{}`", key.name),
                    ));
                }
                return expression;
            };
            for argument in &arguments[definition.arity..] {
                let mut syntax = result.syntax().clone();
                syntax.id = self.fresh_id();
                result = Expression::Call(crate::CallExpression {
                    syntax,
                    callee: Box::new(result),
                    argument: Box::new((*argument).clone()),
                });
            }
            let result = self.expand_expression(module, result, depth + 1);
            self.expansion_stack.pop();
            return result;
        }

        match expression {
            Expression::Function(mut function) => {
                function.body = Box::new(self.expand_expression(module, *function.body, depth));
                Expression::Function(function)
            }
            Expression::Match(mut match_) => {
                match_.subject = Box::new(self.expand_expression(module, *match_.subject, depth));
                for arm in &mut match_.arms {
                    arm.body = self.expand_expression(module, arm.body.clone(), depth);
                }
                Expression::Match(match_)
            }
            Expression::Block(mut block) => {
                for statement in &mut block.statements {
                    self.expand_statement(module, statement, depth);
                }
                Expression::Block(block)
            }
            Expression::Product(mut product) => {
                for element in &mut product.elements {
                    element.value = self.expand_expression(module, element.value.clone(), depth);
                }
                Expression::Product(product)
            }
            Expression::Call(mut call) => {
                call.callee = Box::new(self.expand_expression(module, *call.callee, depth));
                call.argument = Box::new(self.expand_expression(module, *call.argument, depth));
                Expression::Call(call)
            }
            Expression::Access(mut access) => {
                access.value = Box::new(self.expand_expression(module, *access.value, depth));
                Expression::Access(access)
            }
            Expression::Infix(mut infix) => {
                infix.operands = infix
                    .operands
                    .into_iter()
                    .map(|operand| self.expand_expression(module, operand, depth))
                    .collect();
                Expression::Infix(infix)
            }
            Expression::Quote(quote) => {
                self.diagnostics.push(Diagnostic::new(
                    quote.syntax.span.clone(),
                    "`quote` is only available in a macro body or compile-time helper",
                ));
                Expression::Quote(quote)
            }
            Expression::Splice(splice) => {
                self.diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "splices are only available while evaluating `quote`",
                ));
                Expression::Splice(splice)
            }
            other => other,
        }
    }

    fn resolve_macro(&self, module: ModuleId, head: &Expression) -> Option<MacroKey> {
        match head {
            Expression::Name(name) => {
                let context = name
                    .syntax
                    .definition_module()
                    .map(ModuleId)
                    .unwrap_or(module);
                if self.scopes[context.0].helpers.contains_key(&name.name) {
                    None
                } else {
                    self.scopes[context.0].macros.get(&name.name).cloned()
                }
            }
            Expression::Access(access) => {
                let Expression::Name(namespace) = access.value.as_ref() else {
                    return None;
                };
                let Accessor::Name(item) = &access.accessor else {
                    return None;
                };
                let context = namespace
                    .syntax
                    .definition_module()
                    .map(ModuleId)
                    .unwrap_or(module);
                let target = self.scopes[context.0].namespaces.get(&namespace.name)?;
                let key = MacroKey {
                    module: *target,
                    name: item.clone(),
                };
                self.definitions
                    .get(&key)
                    .filter(|definition| definition.declaration.visibility == Visibility::Public)
                    .map(|_| key)
            }
            _ => None,
        }
    }

    fn invoke_macro(
        &mut self,
        definition: &MacroDefinition,
        arguments: Vec<&Expression>,
        call_span: Span,
    ) -> Option<Expression> {
        match &definition.kind {
            MacroKind::CString => {
                let [argument] = arguments.as_slice() else {
                    return None;
                };
                let Expression::String(string) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        argument.syntax().span.clone(),
                        "`c_string` requires a string literal",
                    ));
                    return None;
                };
                let mut syntax = string.syntax.clone();
                syntax.id = self.fresh_id();
                Some(Expression::CString(crate::CStringExpression {
                    syntax,
                    literal: string.literal.clone(),
                }))
            }
            MacroKind::Quote => {
                self.diagnostics.push(Diagnostic::new(
                    call_span,
                    "`quote` may only be evaluated inside a macro body",
                ));
                None
            }
            MacroKind::User(body) => {
                let mut value =
                    self.eval_expression(definition.key.module, body, &mut Environment::new())?;
                for argument in arguments {
                    value = self.apply_value(
                        value,
                        Value::Syntax(argument.clone()),
                        call_span.clone(),
                    )?;
                }
                match value {
                    Value::Syntax(expression) => Some(expression),
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            definition.declaration.syntax.span.clone(),
                            format!("macro `{}` did not return `Syntax`", definition.key.name),
                        ));
                        None
                    }
                }
            }
        }
    }

    fn tick(&mut self, span: Span) -> bool {
        self.steps += 1;
        if self.steps > MAX_EVALUATION_STEPS {
            self.diagnostics.push(Diagnostic::new(
                span,
                "compile-time evaluation exceeded 1000000 steps",
            ));
            false
        } else {
            true
        }
    }

    fn eval_expression(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        environment: &mut Environment,
    ) -> Option<Value> {
        if !self.tick(expression.syntax().span.clone()) {
            return None;
        }
        match expression {
            Expression::Function(function) => Some(Value::Function {
                module,
                function: function.as_ref().clone(),
                environment: environment.clone(),
            }),
            Expression::Quote(quote) => {
                let mark = self.next_mark;
                self.next_mark += 1;
                self.instantiate_quote(module, &quote.template, environment, mark)
                    .map(Value::Syntax)
            }
            Expression::Splice(splice) => {
                self.diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "splices are only allowed inside `quote`",
                ));
                None
            }
            Expression::Name(name) => {
                if let Some(value) = environment.get(&name.name) {
                    return Some(value.clone());
                }
                if let Some(helper) = self.scopes[module.0].helpers.get(&name.name).cloned() {
                    if matches!(helper.binding.value, Some(Expression::Function(_))) {
                        return Some(Value::Helper(helper.module, helper.binding));
                    }
                    let value = helper.binding.value?;
                    return self.eval_expression(helper.module, &value, &mut Environment::new());
                }
                if name.name.chars().next().is_some_and(char::is_uppercase) {
                    return Some(Value::Nominal(
                        name.name.clone(),
                        Box::new(Value::Product(Vec::new())),
                    ));
                }
                self.diagnostics.push(Diagnostic::new(
                    name.syntax.span.clone(),
                    format!("compile-time name `{}` is not available", name.name),
                ));
                None
            }
            Expression::String(string) => Some(Value::String(string.literal.clone())),
            Expression::Integer(integer) => {
                integer.literal.parse::<i128>().ok().map(Value::Integer)
            }
            Expression::Product(product) => {
                let mut values = Vec::new();
                for element in &product.elements {
                    values.push((
                        element.name.clone(),
                        self.eval_expression(module, &element.value, environment)?,
                    ));
                }
                Some(Value::Product(values))
            }
            Expression::Call(call) => {
                let (head, arguments) = flatten_call(expression);
                if let Some(key) = self.resolve_macro(module, head) {
                    let definition = self.definitions.get(&key)?.clone();
                    if arguments.len() != definition.arity {
                        self.diagnostics.push(Diagnostic::new(
                            expression.syntax().span.clone(),
                            format!(
                                "compile-time macro `{}` requires {} arguments",
                                key.name, definition.arity
                            ),
                        ));
                        return None;
                    }
                    if self.expansion_stack.contains(&key) {
                        self.diagnostics.push(Diagnostic::new(
                            expression.syntax().span.clone(),
                            format!("recursive macro expansion of `{}`", key.name),
                        ));
                        return None;
                    }
                    self.expansion_stack.push(key.clone());
                    let result = self
                        .invoke_macro(&definition, arguments, expression.syntax().span.clone())
                        .map(Value::Syntax);
                    self.expansion_stack.pop();
                    return result;
                }
                if let Expression::Name(name) = call.callee.as_ref()
                    && name.name.chars().next().is_some_and(char::is_uppercase)
                {
                    let argument = self.eval_expression(module, &call.argument, environment)?;
                    return Some(Value::Nominal(name.name.clone(), Box::new(argument)));
                }
                let callee = self.eval_expression(module, &call.callee, environment)?;
                let argument = self.eval_expression(module, &call.argument, environment)?;
                self.apply_value(callee, argument, call.syntax.span.clone())
            }
            Expression::Access(access) => {
                if let Expression::Name(namespace) = access.value.as_ref()
                    && let Accessor::Name(item) = &access.accessor
                    && let Some(target) = self.scopes[module.0]
                        .namespaces
                        .get(&namespace.name)
                        .copied()
                    && let Some(helper) = self.scopes[target.0].helpers.get(item).cloned()
                    && helper.binding.visibility == Visibility::Public
                {
                    return Some(Value::Helper(helper.module, helper.binding));
                }
                let value = self.eval_expression(module, &access.value, environment)?;
                let Value::Product(elements) = value else {
                    self.diagnostics.push(Diagnostic::new(
                        access.syntax.span.clone(),
                        "compile-time product access requires a product",
                    ));
                    return None;
                };
                match &access.accessor {
                    Accessor::Index(index) => index
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| elements.get(index).map(|(_, value)| value.clone())),
                    Accessor::Name(name) => elements
                        .into_iter()
                        .find(|(field, _)| field.as_deref() == Some(name))
                        .map(|(_, value)| value),
                }
            }
            Expression::Match(match_) => {
                let subject = self.eval_expression(module, &match_.subject, environment)?;
                for arm in &match_.arms {
                    let mut bindings = Environment::new();
                    if match_pattern(&arm.pattern, &subject, &mut bindings) {
                        let mut arm_environment = environment.clone();
                        arm_environment.extend(bindings);
                        return self.eval_expression(module, &arm.body, &mut arm_environment);
                    }
                }
                self.diagnostics.push(Diagnostic::new(
                    match_.syntax.span.clone(),
                    "compile-time match was not exhaustive",
                ));
                None
            }
            Expression::Block(block) => {
                let mut local = environment.clone();
                let mut result = Value::Product(Vec::new());
                for statement in &block.statements {
                    match statement {
                        Statement::Binding(binding) => {
                            let Some(value) = &binding.value else {
                                self.diagnostics.push(Diagnostic::new(
                                    binding.syntax.span.clone(),
                                    "compile-time declarations require a value",
                                ));
                                return None;
                            };
                            let value = self.eval_expression(module, value, &mut local)?;
                            local.insert(binding.name.clone(), value);
                        }
                        Statement::PatternBinding(binding) => {
                            let value = self.eval_expression(module, &binding.value, &mut local)?;
                            if !match_pattern(&binding.pattern, &value, &mut local) {
                                return None;
                            }
                        }
                        Statement::Expression(value) => {
                            result = self.eval_expression(module, value, &mut local)?
                        }
                        Statement::Return(return_) => {
                            return self.eval_expression(module, &return_.value, &mut local);
                        }
                    }
                }
                Some(result)
            }
            Expression::Infix(infix) => self.eval_infix(module, infix, environment),
            Expression::CString(_) => {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    "C strings are not compile-time values",
                ));
                None
            }
        }
    }

    fn apply_value(&mut self, callee: Value, argument: Value, span: Span) -> Option<Value> {
        match callee {
            Value::Function {
                module,
                function,
                mut environment,
            } => {
                if !bind_pattern(&function.pattern, argument, &mut environment) {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "compile-time function argument does not match its parameter",
                    ));
                    return None;
                }
                self.eval_expression(module, &function.body, &mut environment)
            }
            Value::Helper(module, binding) => {
                let value = binding.value?;
                let function = self.eval_expression(module, &value, &mut Environment::new())?;
                self.apply_value(function, argument, span)
            }
            _ => {
                self.diagnostics
                    .push(Diagnostic::new(span, "cannot call this compile-time value"));
                None
            }
        }
    }

    fn eval_infix(
        &mut self,
        module: ModuleId,
        infix: &crate::InfixExpression,
        environment: &mut Environment,
    ) -> Option<Value> {
        let mut value = self.eval_expression(module, infix.operands.first()?, environment)?;
        for (operator, operand) in infix.operators.iter().zip(&infix.operands[1..]) {
            let right = self.eval_expression(module, operand, environment)?;
            value = match (&value, &right, operator.name.as_str()) {
                (Value::Integer(left), Value::Integer(right), "+") => {
                    Value::Integer(left.wrapping_add(*right))
                }
                (Value::Integer(left), Value::Integer(right), "-") => {
                    Value::Integer(left.wrapping_sub(*right))
                }
                (Value::Integer(left), Value::Integer(right), "*") => {
                    Value::Integer(left.wrapping_mul(*right))
                }
                (Value::Integer(left), Value::Integer(right), "/") if *right != 0 => {
                    Value::Integer(left.wrapping_div(*right))
                }
                (Value::Integer(left), Value::Integer(right), "==") => bool_value(left == right),
                (Value::Integer(left), Value::Integer(right), "!=") => bool_value(left != right),
                (Value::Integer(left), Value::Integer(right), "<") => bool_value(left < right),
                (Value::Integer(left), Value::Integer(right), "<=") => bool_value(left <= right),
                (Value::Integer(left), Value::Integer(right), ">") => bool_value(left > right),
                (Value::Integer(left), Value::Integer(right), ">=") => bool_value(left >= right),
                (Value::String(left), Value::String(right), "==") => bool_value(left == right),
                (Value::String(left), Value::String(right), "!=") => bool_value(left != right),
                _ => {
                    self.diagnostics.push(Diagnostic::new(
                        operator.syntax.span.clone(),
                        format!(
                            "operator `{}` is not available for these compile-time values",
                            operator.name
                        ),
                    ));
                    return None;
                }
            };
        }
        Some(value)
    }

    fn instantiate_quote(
        &mut self,
        module: ModuleId,
        template: &Expression,
        environment: &Environment,
        mark: u64,
    ) -> Option<Expression> {
        let mut expression = template.clone();
        alpha_rename_expression(&mut expression, mark, &mut Vec::new());
        self.freshen_expression(&mut expression, module, mark);
        substitute_splices(&expression, environment, &mut self.diagnostics)
    }

    fn fresh_id(&mut self) -> SyntaxId {
        let id = SyntaxId(self.next_syntax_id);
        self.next_syntax_id += 1;
        id
    }

    fn freshen_syntax(&mut self, syntax: &mut Syntax, module: ModuleId, mark: u64) {
        syntax.id = self.fresh_id();
        *syntax = syntax.clone().generated(module.0, mark);
    }

    fn freshen_expression(&mut self, expression: &mut Expression, module: ModuleId, mark: u64) {
        self.freshen_syntax(expression_syntax_mut(expression), module, mark);
        match expression {
            Expression::Function(function) => {
                freshen_pattern(self, &mut function.pattern, module, mark);
                if let Some(return_type) = &mut function.return_type {
                    freshen_type(self, return_type, module, mark);
                }
                self.freshen_expression(&mut function.body, module, mark);
            }
            Expression::Match(match_) => {
                self.freshen_expression(&mut match_.subject, module, mark);
                for arm in &mut match_.arms {
                    self.freshen_syntax(&mut arm.syntax, module, mark);
                    freshen_pattern(self, &mut arm.pattern, module, mark);
                    self.freshen_expression(&mut arm.body, module, mark);
                }
            }
            Expression::Block(block) => {
                for statement in &mut block.statements {
                    freshen_statement(self, statement, module, mark);
                }
            }
            Expression::Product(product) => {
                for element in &mut product.elements {
                    self.freshen_syntax(&mut element.syntax, module, mark);
                    self.freshen_expression(&mut element.value, module, mark);
                }
            }
            Expression::Call(call) => {
                self.freshen_expression(&mut call.callee, module, mark);
                self.freshen_expression(&mut call.argument, module, mark);
            }
            Expression::Access(access) => self.freshen_expression(&mut access.value, module, mark),
            Expression::Infix(infix) => {
                for operand in &mut infix.operands {
                    self.freshen_expression(operand, module, mark);
                }
                for operator in &mut infix.operators {
                    self.freshen_syntax(&mut operator.syntax, module, mark);
                }
            }
            Expression::Quote(_) => {}
            Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_) => {}
        }
    }
}

fn expression_arity(expression: &Expression) -> usize {
    match expression {
        Expression::Function(function) => 1 + expression_arity(&function.body),
        _ => 0,
    }
}

fn macro_annotation_arity(annotation: &Type) -> usize {
    match annotation {
        Type::Function(function) => 1 + macro_annotation_arity(&function.result),
        _ => 0,
    }
}

fn is_syntax_type(ty: &Type) -> bool {
    matches!(ty, Type::Named(named) if named.namespace.is_none() && named.name == "Syntax")
}

fn type_contains_syntax(ty: &Type) -> bool {
    match ty {
        Type::Named(named) => named.name == "Syntax",
        Type::Function(function) => {
            type_contains_syntax(&function.parameter) || type_contains_syntax(&function.result)
        }
        Type::Product(product) => product
            .elements
            .iter()
            .any(|element| type_contains_syntax(&element.ty)),
        Type::Sum(sum) => sum.alternatives.iter().any(type_contains_syntax),
        Type::Application(application) => {
            type_contains_syntax(&application.callee) || type_contains_syntax(&application.argument)
        }
        Type::Inferred(_) => false,
    }
}

fn binding_is_compile_time_helper(binding: &Binding) -> bool {
    binding.kind == BindingKind::Def && binding_contains_syntax(binding)
}

fn binding_contains_syntax(binding: &Binding) -> bool {
    binding
        .annotation
        .as_ref()
        .is_some_and(type_contains_syntax)
        || binding
            .value
            .as_ref()
            .is_some_and(expression_parameter_contains_syntax)
}

fn expression_parameter_contains_syntax(expression: &Expression) -> bool {
    let Expression::Function(function) = expression else {
        return false;
    };
    pattern_contains_syntax(&function.pattern)
        || expression_parameter_contains_syntax(&function.body)
}

fn pattern_contains_syntax(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Binding(binding) => type_contains_syntax(&binding.ty),
        Pattern::Product(product) => product.elements.iter().any(pattern_contains_syntax),
        Pattern::Nominal(pattern) => pattern_contains_syntax(&pattern.argument),
        Pattern::Wildcard(_) => false,
    }
}

fn obviously_not_syntax(expression: &Expression, arity: usize) -> bool {
    if arity > 0 {
        return match expression {
            Expression::Function(function) => obviously_not_syntax(&function.body, arity - 1),
            _ => true,
        };
    }
    match expression {
        Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::Name(_)
        | Expression::Call(_)
        | Expression::Access(_) => false,
        Expression::Match(match_) => match_
            .arms
            .iter()
            .any(|arm| obviously_not_syntax(&arm.body, 0)),
        Expression::Block(block) => {
            block
                .statements
                .last()
                .is_none_or(|statement| match statement {
                    Statement::Expression(expression) => obviously_not_syntax(expression, 0),
                    Statement::Return(return_) => obviously_not_syntax(&return_.value, 0),
                    Statement::Binding(_) | Statement::PatternBinding(_) => true,
                })
        }
        Expression::Infix(_)
        | Expression::Function(_)
        | Expression::Product(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_) => true,
    }
}

fn valid_macro_annotation(annotation: &Type) -> bool {
    match annotation {
        Type::Function(function) => {
            is_syntax_type(&function.parameter)
                && match function.result.as_ref() {
                    Type::Function(_) => valid_macro_annotation(&function.result),
                    result => is_syntax_type(result),
                }
        }
        _ => false,
    }
}

fn valid_macro_parameter_patterns(expression: &Expression, arity: usize) -> bool {
    if arity == 0 {
        return true;
    }
    let Expression::Function(function) = expression else {
        return false;
    };
    let valid_pattern = match &function.pattern {
        Pattern::Binding(binding) => {
            matches!(binding.ty, Type::Inferred(_)) || is_syntax_type(&binding.ty)
        }
        Pattern::Wildcard(_) => true,
        Pattern::Product(_) | Pattern::Nominal(_) => false,
    };
    valid_pattern && valid_macro_parameter_patterns(&function.body, arity - 1)
}

fn flatten_call(expression: &Expression) -> (&Expression, Vec<&Expression>) {
    let mut arguments = Vec::new();
    let mut head = expression;
    while let Expression::Call(call) = head {
        arguments.push(call.argument.as_ref());
        head = call.callee.as_ref();
    }
    arguments.reverse();
    (head, arguments)
}

fn bool_value(value: bool) -> Value {
    Value::Nominal(
        if value { "True" } else { "False" }.to_owned(),
        Box::new(Value::Product(Vec::new())),
    )
}

fn bind_pattern(pattern: &Pattern, value: Value, environment: &mut Environment) -> bool {
    match pattern {
        Pattern::Wildcard(_) => true,
        Pattern::Binding(binding) => {
            environment.insert(binding.name.clone(), value);
            true
        }
        Pattern::Product(product) => {
            let Value::Product(values) = value else {
                return false;
            };
            product.elements.len() == values.len()
                && product
                    .elements
                    .iter()
                    .zip(values)
                    .all(|(pattern, (_, value))| bind_pattern(pattern, value, environment))
        }
        Pattern::Nominal(pattern) => {
            let Value::Nominal(name, value) = value else {
                return false;
            };
            pattern.name == name && bind_pattern(&pattern.argument, *value, environment)
        }
    }
}

fn match_pattern(pattern: &Pattern, value: &Value, environment: &mut Environment) -> bool {
    bind_pattern(pattern, value.clone(), environment)
}

fn substitute_splices(
    expression: &Expression,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Expression> {
    if let Expression::Splice(splice) = expression {
        if splice.repeated {
            diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "repeated splices are not supported yet",
            ));
            return None;
        }
        return match environment.get(&splice.name) {
            Some(Value::Syntax(expression)) => Some(expression.clone()),
            Some(_) => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!("splice `${}` does not contain `Syntax`", splice.name),
                ));
                None
            }
            None => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!("unknown splice `${}`", splice.name),
                ));
                None
            }
        };
    }
    let mut result = expression.clone();
    match &mut result {
        Expression::Function(function) => {
            *function.body = substitute_splices(&function.body, environment, diagnostics)?
        }
        Expression::Match(match_) => {
            *match_.subject = substitute_splices(&match_.subject, environment, diagnostics)?;
            for arm in &mut match_.arms {
                arm.body = substitute_splices(&arm.body, environment, diagnostics)?;
            }
        }
        Expression::Block(block) => {
            for statement in &mut block.statements {
                substitute_statement(statement, environment, diagnostics)?;
            }
        }
        Expression::Product(product) => {
            for element in &mut product.elements {
                element.value = substitute_splices(&element.value, environment, diagnostics)?;
            }
        }
        Expression::Call(call) => {
            *call.callee = substitute_splices(&call.callee, environment, diagnostics)?;
            *call.argument = substitute_splices(&call.argument, environment, diagnostics)?;
        }
        Expression::Access(access) => {
            *access.value = substitute_splices(&access.value, environment, diagnostics)?
        }
        Expression::Infix(infix) => {
            for operand in &mut infix.operands {
                *operand = substitute_splices(operand, environment, diagnostics)?;
            }
        }
        Expression::Quote(_) => {}
        Expression::Splice(_)
        | Expression::Name(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_) => {}
    }
    Some(result)
}

fn substitute_statement(
    statement: &mut Statement,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    match statement {
        Statement::Binding(binding) => {
            if let Some(value) = &mut binding.value {
                *value = substitute_splices(value, environment, diagnostics)?;
            }
        }
        Statement::PatternBinding(binding) => {
            binding.value = substitute_splices(&binding.value, environment, diagnostics)?
        }
        Statement::Return(return_) => {
            return_.value = substitute_splices(&return_.value, environment, diagnostics)?
        }
        Statement::Expression(expression) => {
            *expression = substitute_splices(expression, environment, diagnostics)?
        }
    }
    Some(())
}

fn hygienic_name(name: &str, mark: u64) -> String {
    format!("{name}__macro_{mark}")
}

fn alpha_rename_pattern(pattern: &mut Pattern, mark: u64, names: &mut HashMap<String, String>) {
    match pattern {
        Pattern::Binding(binding) => {
            let renamed = hygienic_name(&binding.name, mark);
            names.insert(binding.name.clone(), renamed.clone());
            binding.name = renamed;
        }
        Pattern::Product(product) => {
            for element in &mut product.elements {
                alpha_rename_pattern(element, mark, names);
            }
        }
        Pattern::Nominal(pattern) => alpha_rename_pattern(&mut pattern.argument, mark, names),
        Pattern::Wildcard(_) => {}
    }
}

fn alpha_rename_expression(
    expression: &mut Expression,
    mark: u64,
    scopes: &mut Vec<HashMap<String, String>>,
) {
    match expression {
        Expression::Name(name) => {
            if let Some(renamed) = scopes.iter().rev().find_map(|scope| scope.get(&name.name)) {
                name.name = renamed.clone();
            }
        }
        Expression::Function(function) => {
            let mut scope = HashMap::new();
            alpha_rename_pattern(&mut function.pattern, mark, &mut scope);
            scopes.push(scope);
            alpha_rename_expression(&mut function.body, mark, scopes);
            scopes.pop();
        }
        Expression::Match(match_) => {
            alpha_rename_expression(&mut match_.subject, mark, scopes);
            for arm in &mut match_.arms {
                let mut scope = HashMap::new();
                alpha_rename_pattern(&mut arm.pattern, mark, &mut scope);
                scopes.push(scope);
                alpha_rename_expression(&mut arm.body, mark, scopes);
                scopes.pop();
            }
        }
        Expression::Block(block) => {
            scopes.push(HashMap::new());
            for statement in &mut block.statements {
                match statement {
                    Statement::Binding(binding) => {
                        if let Some(value) = &mut binding.value {
                            alpha_rename_expression(value, mark, scopes);
                        }
                        let renamed = hygienic_name(&binding.name, mark);
                        scopes
                            .last_mut()
                            .unwrap()
                            .insert(binding.name.clone(), renamed.clone());
                        binding.name = renamed;
                    }
                    Statement::PatternBinding(binding) => {
                        alpha_rename_expression(&mut binding.value, mark, scopes);
                        alpha_rename_pattern(
                            &mut binding.pattern,
                            mark,
                            scopes.last_mut().unwrap(),
                        );
                    }
                    Statement::Return(return_) => {
                        alpha_rename_expression(&mut return_.value, mark, scopes)
                    }
                    Statement::Expression(expression) => {
                        alpha_rename_expression(expression, mark, scopes)
                    }
                }
            }
            scopes.pop();
        }
        Expression::Product(product) => {
            for element in &mut product.elements {
                alpha_rename_expression(&mut element.value, mark, scopes);
            }
        }
        Expression::Call(call) => {
            alpha_rename_expression(&mut call.callee, mark, scopes);
            alpha_rename_expression(&mut call.argument, mark, scopes);
        }
        Expression::Access(access) => alpha_rename_expression(&mut access.value, mark, scopes),
        Expression::Infix(infix) => {
            for operand in &mut infix.operands {
                alpha_rename_expression(operand, mark, scopes);
            }
        }
        Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_) => {}
    }
}

fn expression_syntax_mut(expression: &mut Expression) -> &mut Syntax {
    match expression {
        Expression::Function(value) => &mut value.syntax,
        Expression::Match(value) => &mut value.syntax,
        Expression::Block(value) => &mut value.syntax,
        Expression::Product(value) => &mut value.syntax,
        Expression::Call(value) => &mut value.syntax,
        Expression::Access(value) => &mut value.syntax,
        Expression::Infix(value) => &mut value.syntax,
        Expression::Quote(value) => &mut value.syntax,
        Expression::Splice(value) => &mut value.syntax,
        Expression::Name(value) => &mut value.syntax,
        Expression::String(value) => &mut value.syntax,
        Expression::CString(value) => &mut value.syntax,
        Expression::Integer(value) => &mut value.syntax,
    }
}

fn freshen_pattern(
    expander: &mut MacroExpander,
    pattern: &mut Pattern,
    module: ModuleId,
    mark: u64,
) {
    match pattern {
        Pattern::Binding(binding) => {
            expander.freshen_syntax(&mut binding.syntax, module, mark);
            freshen_type(expander, &mut binding.ty, module, mark);
        }
        Pattern::Wildcard(wildcard) => expander.freshen_syntax(&mut wildcard.syntax, module, mark),
        Pattern::Product(product) => {
            expander.freshen_syntax(&mut product.syntax, module, mark);
            for element in &mut product.elements {
                freshen_pattern(expander, element, module, mark);
            }
        }
        Pattern::Nominal(nominal) => {
            expander.freshen_syntax(&mut nominal.syntax, module, mark);
            freshen_pattern(expander, &mut nominal.argument, module, mark);
        }
    }
}

fn freshen_statement(
    expander: &mut MacroExpander,
    statement: &mut Statement,
    module: ModuleId,
    mark: u64,
) {
    match statement {
        Statement::Binding(binding) => {
            expander.freshen_syntax(&mut binding.syntax, module, mark);
            if let Some(annotation) = &mut binding.annotation {
                freshen_type(expander, annotation, module, mark);
            }
            if let Some(value) = &mut binding.value {
                expander.freshen_expression(value, module, mark);
            }
        }
        Statement::PatternBinding(binding) => {
            expander.freshen_syntax(&mut binding.syntax, module, mark);
            freshen_pattern(expander, &mut binding.pattern, module, mark);
            expander.freshen_expression(&mut binding.value, module, mark);
        }
        Statement::Return(return_) => {
            expander.freshen_syntax(&mut return_.syntax, module, mark);
            expander.freshen_expression(&mut return_.value, module, mark);
        }
        Statement::Expression(expression) => expander.freshen_expression(expression, module, mark),
    }
}

fn freshen_type(expander: &mut MacroExpander, ty: &mut Type, module: ModuleId, mark: u64) {
    let syntax = match ty {
        Type::Inferred(ty) => &mut ty.syntax,
        Type::Named(ty) => &mut ty.syntax,
        Type::Product(ty) => &mut ty.syntax,
        Type::Sum(ty) => &mut ty.syntax,
        Type::Function(ty) => &mut ty.syntax,
        Type::Application(ty) => &mut ty.syntax,
    };
    expander.freshen_syntax(syntax, module, mark);
    match ty {
        Type::Product(product) => {
            for element in &mut product.elements {
                expander.freshen_syntax(&mut element.syntax, module, mark);
                freshen_type(expander, &mut element.ty, module, mark);
            }
        }
        Type::Sum(sum) => {
            for alternative in &mut sum.alternatives {
                freshen_type(expander, alternative, module, mark);
            }
        }
        Type::Function(function) => {
            freshen_type(expander, &mut function.parameter, module, mark);
            freshen_type(expander, &mut function.result, module, mark);
        }
        Type::Application(application) => {
            freshen_type(expander, &mut application.callee, module, mark);
            freshen_type(expander, &mut application.argument, module, mark);
        }
        Type::Inferred(_) | Type::Named(_) => {}
    }
}
