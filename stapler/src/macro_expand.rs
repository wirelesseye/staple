use std::collections::HashMap;

use crate::{
    Accessor, Binding, BindingKind, BlockExpression, Diagnostic, Expression, Item,
    MacroDeclaration, ModuleId, Pattern, Program, Span, Statement, Syntax, SyntaxId, Type, UseKind,
    Visibility,
};

const MAX_EXPANSION_DEPTH: usize = 128;
const MAX_EVALUATION_STEPS: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MacroKey {
    module: ModuleId,
    name: String,
    syntax: SyntaxId,
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
    parameters: Vec<MetaType>,
    result: MetaType,
    kind: MacroKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MetaType {
    Syntax,
    Expr,
    Ident(Option<String>),
    Type,
    Pattern,
    Item,
}

#[derive(Clone, Default)]
struct ModuleScope {
    macros: HashMap<String, Vec<MacroKey>>,
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
                            syntax: declaration.syntax.id,
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
                        let (parameters, result) = declaration
                            .annotation
                            .as_ref()
                            .and_then(macro_signature)
                            .unwrap_or_else(|| {
                                inferred_macro_signature(declaration.value.as_ref())
                            });
                        definitions.insert(
                            key.clone(),
                            MacroDefinition {
                                key: key.clone(),
                                declaration: declaration.clone(),
                                arity,
                                parameters,
                                result,
                                kind,
                            },
                        );
                        scopes[source_module.id.0]
                            .macros
                            .entry(declaration.name.clone())
                            .or_default()
                            .push(key);
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

        let mut public_macros = HashMap::<(ModuleId, String), Vec<MacroKey>>::new();
        for definition in definitions
            .values()
            .filter(|definition| definition.declaration.visibility == Visibility::Public)
        {
            public_macros
                .entry((definition.key.module, definition.key.name.clone()))
                .or_default()
                .push(definition.key.clone());
        }
        let mut public_helpers = program
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

        loop {
            let previous_macros = public_macros.clone();
            let previous_helpers = public_helpers.clone();
            let mut changed = false;
            for source_module in program.modules() {
                for item in &source_module.syntax.items {
                    let Item::UseDeclaration(use_) = item else {
                        continue;
                    };
                    if use_.visibility != Visibility::Public {
                        continue;
                    }
                    let Some(imported) = program.imported_module(use_.syntax.id) else {
                        continue;
                    };
                    let names = match &use_.kind {
                        UseKind::Namespace => Vec::new(),
                        UseKind::Glob => previous_macros
                            .keys()
                            .filter(|(module, _)| *module == imported)
                            .map(|(_, name)| (name.clone(), name.clone()))
                            .chain(
                                previous_helpers
                                    .keys()
                                    .filter(|(module, _)| *module == imported)
                                    .map(|(_, name)| (name.clone(), name.clone())),
                            )
                            .collect(),
                        UseKind::Selected(names) => names
                            .iter()
                            .map(|name| (name.clone(), name.clone()))
                            .collect(),
                        UseKind::Renamed { item, alias } => {
                            vec![(item.clone(), alias.clone())]
                        }
                    };
                    for (item, alias) in names {
                        if let Some(keys) = previous_macros.get(&(imported, item.clone())) {
                            changed |= public_macros
                                .insert((source_module.id, alias.clone()), keys.clone())
                                .is_none();
                        }
                        if let Some(helper) = previous_helpers.get(&(imported, item)) {
                            changed |= public_helpers
                                .insert((source_module.id, alias), helper.clone())
                                .is_none();
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }

        let mut all_macros = HashMap::<(ModuleId, String), Vec<MacroKey>>::new();
        for definition in definitions.values() {
            all_macros
                .entry((definition.key.module, definition.key.name.clone()))
                .or_default()
                .push(definition.key.clone());
        }
        let all_helpers = program
            .modules()
            .iter()
            .flat_map(|module| {
                scopes[module.id.0]
                    .helpers
                    .iter()
                    .map(move |(name, helper)| ((module.id, name.clone()), helper.clone()))
            })
            .collect::<HashMap<_, _>>();

        for source_module in program.modules() {
            if let Some(core) = core
                && source_module.id != core
            {
                for ((module, name), keys) in &public_macros {
                    if *module != core {
                        continue;
                    }
                    scopes[source_module.id.0]
                        .macros
                        .entry(name.clone())
                        .or_insert_with(|| keys.clone());
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
                if let Item::Submodule(submodule) = item
                    && let Some(child) = program.child_module(submodule.syntax.id)
                {
                    scopes[source_module.id.0]
                        .namespaces
                        .insert(submodule.name.clone(), child);
                }
            }
            for item in &source_module.syntax.items {
                let Item::UseDeclaration(use_) = item else {
                    continue;
                };
                let Some(imported) = program.imported_module(use_.syntax.id) else {
                    continue;
                };
                let (macros, helpers) = if use_.visibility == Visibility::Private
                    && macro_is_ancestor(program, imported, source_module.id)
                {
                    (&all_macros, &all_helpers)
                } else {
                    (&public_macros, &public_helpers)
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
                        for ((_, name), keys) in
                            macros.iter().filter(|((module, _), _)| *module == imported)
                        {
                            scopes[source_module.id.0]
                                .macros
                                .entry(name.clone())
                                .or_insert_with(|| keys.clone());
                        }
                        for ((module, name), binding) in helpers {
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
                                macros,
                                helpers,
                            );
                        }
                    }
                    UseKind::Renamed { item, alias } => Self::install_selected(
                        &mut scopes[source_module.id.0],
                        imported,
                        item,
                        alias,
                        macros,
                        helpers,
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
        public_macros: &HashMap<(ModuleId, String), Vec<MacroKey>>,
        public_helpers: &HashMap<(ModuleId, String), HelperDefinition>,
    ) {
        if let Some(keys) = public_macros.get(&(imported, item.to_owned())) {
            scope
                .macros
                .entry(local.to_owned())
                .or_insert_with(|| keys.clone());
        }
        if let Some(helper) = public_helpers.get(&(imported, item.to_owned())) {
            scope
                .helpers
                .entry(local.to_owned())
                .or_insert_with(|| helper.clone());
        }
    }

    fn validate_definitions(&mut self) {
        let mut groups = HashMap::<(ModuleId, String), Vec<MacroDefinition>>::new();
        for definition in self.definitions.values() {
            groups
                .entry((definition.key.module, definition.key.name.clone()))
                .or_default()
                .push(definition.clone());
        }
        for ((_, name), mut definitions) in groups {
            definitions.sort_by_key(|definition| definition.key.syntax.0);
            for (index, definition) in definitions.iter().enumerate() {
                if let Some(previous) = definitions[..index]
                    .iter()
                    .find(|previous| previous.parameters == definition.parameters)
                {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "duplicate macro overload `{name}: {}`",
                            format_meta_signature(&definition.parameters)
                        ),
                    ));
                    self.diagnostics.push(Diagnostic::new(
                        previous.declaration.syntax.span.clone(),
                        "previous overload defined here",
                    ));
                }
            }
        }
        for definition in self.definitions.values() {
            if let Some(annotation) = &definition.declaration.annotation
                && !valid_macro_annotation(annotation)
            {
                self.diagnostics.push(Diagnostic::new(
                    annotation.syntax().span.clone(),
                    "a macro annotation must accept one or more syntax-category parameters and return a syntax category",
                ));
            }
            if let (Some(annotation), Some(body)) = (
                definition.declaration.annotation.as_ref(),
                definition.declaration.value.as_ref(),
            ) && let Some((parameters, _)) = macro_signature(annotation)
            {
                for (index, (declared, body_parameter)) in parameters
                    .iter()
                    .zip(macro_body_parameter_types(body))
                    .enumerate()
                {
                    if body_parameter.is_some_and(|body_parameter| body_parameter != *declared) {
                        self.diagnostics.push(Diagnostic::new(
                            body.syntax().span.clone(),
                            format!(
                                "macro `{}` parameter {} conflicts with its annotation",
                                definition.key.name,
                                index + 1
                            ),
                        ));
                    }
                }
            }
            match &definition.kind {
                MacroKind::CString
                    if definition.parameters != [MetaType::Expr]
                        || definition.result != MetaType::Expr =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        "compiler-provided macro `c_string` must have signature `Expr -> Expr`",
                    ));
                }
                MacroKind::Quote
                    if definition.parameters != [MetaType::Syntax]
                        || definition.result != MetaType::Syntax =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        "compiler-provided macro `quote` must have signature `Syntax -> Syntax`",
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
                            "macro `{}` parameters must match atomic syntax values",
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
            Item::TraitDeclaration(declaration) => {
                for member in &mut declaration.members {
                    if let Some(default) = member.default.take() {
                        member.default = Some(self.expand_expression(module, default, 0));
                    }
                }
            }
            Item::MacroDeclaration(_)
            | Item::Submodule(_)
            | Item::UseDeclaration(_)
            | Item::ExternBlock(_)
            | Item::TypeDeclaration(_) => {}
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
            Statement::Assignment(assignment) => {
                assignment.target =
                    self.expand_expression(module, assignment.target.clone(), depth);
                assignment.value = self.expand_expression(module, assignment.value.clone(), depth);
            }
            Statement::Return(return_) => {
                return_.value = self.expand_expression(module, return_.value.clone(), depth);
            }
            Statement::Break(break_) => {
                if let Some(value) = break_.value.take() {
                    break_.value = Some(self.expand_expression(module, value, depth));
                }
            }
            Statement::Continue(_) => {}
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
        if let Some(keys) = self.resolve_macro(module, head) {
            let Some(definition) =
                self.select_macro(&keys, &arguments, expression.syntax().span.clone())
            else {
                return expression;
            };
            let key = definition.key.clone();
            if !matches!(definition.result, MetaType::Syntax | MetaType::Expr) {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    format!("macro `{}` does not produce expression syntax", key.name),
                ));
                return expression;
            }
            if self.expansion_stack.contains(&key) && head.syntax().definition_module().is_some() {
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
            Expression::Satisfies(mut satisfies) => {
                satisfies.value = Box::new(self.expand_expression(module, *satisfies.value, depth));
                Expression::Satisfies(satisfies)
            }
            Expression::Match(mut match_) => {
                match_.subject = Box::new(self.expand_expression(module, *match_.subject, depth));
                for arm in &mut match_.arms {
                    arm.body = self.expand_expression(module, arm.body.clone(), depth);
                }
                Expression::Match(match_)
            }
            Expression::Loop(mut loop_) => {
                for statement in &mut loop_.body.statements {
                    self.expand_statement(module, statement, depth);
                }
                Expression::Loop(loop_)
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
            Expression::Index(mut index) => {
                index.value = Box::new(self.expand_expression(module, *index.value, depth));
                index.index = Box::new(self.expand_expression(module, *index.index, depth));
                Expression::Index(index)
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

    fn resolve_macro(&self, module: ModuleId, head: &Expression) -> Option<Vec<MacroKey>> {
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
                self.scopes[target.0].macros.get(item).map(|keys| {
                    keys.iter()
                        .filter(|key| {
                            self.definitions[key].declaration.visibility == Visibility::Public
                        })
                        .cloned()
                        .collect()
                })
            }
            _ => None,
        }
    }

    fn select_macro(
        &mut self,
        keys: &[MacroKey],
        arguments: &[&Expression],
        span: Span,
    ) -> Option<MacroDefinition> {
        let definitions = keys
            .iter()
            .filter_map(|key| self.definitions.get(key).cloned())
            .collect::<Vec<_>>();
        let mut complete = definitions
            .iter()
            .filter(|definition| {
                arguments.len() >= definition.arity
                    && definition
                        .parameters
                        .iter()
                        .zip(arguments)
                        .all(|(expected, argument)| meta_type_matches(expected, argument))
            })
            .cloned()
            .collect::<Vec<_>>();
        if complete.is_empty() {
            if let [definition] = definitions.as_slice()
                && arguments.len() >= definition.arity
            {
                self.validate_macro_arguments(definition, &arguments[..definition.arity]);
                return None;
            }
            let mut arities = definitions
                .iter()
                .filter(|definition| {
                    arguments.len() < definition.arity
                        && definition
                            .parameters
                            .iter()
                            .zip(arguments)
                            .all(|(expected, argument)| meta_type_matches(expected, argument))
                })
                .map(|definition| definition.arity)
                .collect::<Vec<_>>();
            arities.sort_unstable();
            arities.dedup();
            let name = keys
                .first()
                .map(|key| key.name.as_str())
                .unwrap_or("<macro>");
            let message = if arities.is_empty() {
                format!("no overload of macro `{name}` matches this invocation")
            } else if let [arity] = arities.as_slice() {
                format!(
                    "macro `{name}` requires {arity} argument{} but received {}",
                    if *arity == 1 { "" } else { "s" },
                    arguments.len()
                )
            } else {
                let required = arities
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(" or ");
                format!(
                    "macro `{name}` received {} argument{}; matching overloads require {required}",
                    arguments.len(),
                    if arguments.len() == 1 { "" } else { "s" }
                )
            };
            self.diagnostics.push(Diagnostic::new(span, message));
            return None;
        }
        let longest = complete.iter().map(|definition| definition.arity).max()?;
        complete.retain(|definition| definition.arity == longest);
        let undominated = complete
            .iter()
            .filter(|candidate| {
                !complete.iter().any(|other| {
                    other.key != candidate.key
                        && signature_more_specific(&other.parameters, &candidate.parameters)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if let [definition] = undominated.as_slice() {
            return Some(definition.clone());
        }
        let name = keys
            .first()
            .map(|key| key.name.as_str())
            .unwrap_or("<macro>");
        self.diagnostics.push(Diagnostic::new(
            span,
            format!("ambiguous invocation of macro `{name}`"),
        ));
        for definition in undominated {
            self.diagnostics.push(Diagnostic::new(
                definition.declaration.syntax.span.clone(),
                format!(
                    "matching overload `{name}: {}` defined here",
                    format_meta_signature(&definition.parameters)
                ),
            ));
        }
        None
    }

    fn invoke_macro(
        &mut self,
        definition: &MacroDefinition,
        arguments: Vec<&Expression>,
        call_span: Span,
    ) -> Option<Expression> {
        if !self.validate_macro_arguments(definition, &arguments) {
            return None;
        }
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

    fn validate_macro_arguments(
        &mut self,
        definition: &MacroDefinition,
        arguments: &[&Expression],
    ) -> bool {
        let mut valid = true;
        for (index, (expected, argument)) in definition.parameters.iter().zip(arguments).enumerate()
        {
            let matches = meta_type_matches(expected, argument);
            if !matches {
                let expectation = match expected {
                    MetaType::Ident(Some(spelling)) => {
                        format!("identifier `{spelling}`")
                    }
                    MetaType::Ident(None) => "an identifier".to_owned(),
                    MetaType::Type => "a type".to_owned(),
                    MetaType::Pattern => "a pattern".to_owned(),
                    MetaType::Item => "an item".to_owned(),
                    MetaType::Syntax | MetaType::Expr => "an expression".to_owned(),
                };
                self.diagnostics.push(Diagnostic::new(
                    argument.syntax().span.clone(),
                    format!(
                        "argument {} of macro `{}` must be {expectation}",
                        index + 1,
                        definition.key.name
                    ),
                ));
                valid = false;
            }
        }
        valid
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
            Expression::Satisfies(satisfies) => {
                self.eval_expression(module, &satisfies.value, environment)
            }
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
            Expression::Float(float) => {
                self.diagnostics.push(Diagnostic::new(
                    float.syntax.span.clone(),
                    "float literals are not supported in compile-time evaluation",
                ));
                None
            }
            Expression::Product(product) => {
                let mut values = Vec::new();
                for element in &product.elements {
                    let value = self.eval_expression(module, &element.value, environment)?;
                    if element.spread {
                        let Value::Product(elements) = value else {
                            self.diagnostics.push(Diagnostic::new(
                                element.syntax.span.clone(),
                                "compile-time product spread requires a product value",
                            ));
                            return None;
                        };
                        values.extend(elements);
                    } else {
                        values.push((element.name.clone(), value));
                    }
                }
                Some(Value::Product(values))
            }
            Expression::Call(call) => {
                let (head, arguments) = flatten_call(expression);
                if let Some(keys) = self.resolve_macro(module, head) {
                    let definition =
                        self.select_macro(&keys, &arguments, expression.syntax().span.clone())?;
                    let key = definition.key.clone();
                    if self.expansion_stack.contains(&key) {
                        self.diagnostics.push(Diagnostic::new(
                            expression.syntax().span.clone(),
                            format!("recursive macro expansion of `{}`", key.name),
                        ));
                        return None;
                    }
                    self.expansion_stack.push(key.clone());
                    let consumed = arguments[..definition.arity].to_vec();
                    let mut result =
                        self.invoke_macro(&definition, consumed, expression.syntax().span.clone());
                    if let Some(mut expanded) = result.take() {
                        for argument in &arguments[definition.arity..] {
                            let mut syntax = expanded.syntax().clone();
                            syntax.id = self.fresh_id();
                            expanded = Expression::Call(crate::CallExpression {
                                syntax,
                                callee: Box::new(expanded),
                                argument: Box::new((*argument).clone()),
                            });
                        }
                        result = Some(expanded);
                    }
                    let result = result.map(Value::Syntax);
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
            Expression::Index(index) => {
                let value = self.eval_expression(module, &index.value, environment)?;
                let Value::Product(elements) = value else {
                    self.diagnostics.push(Diagnostic::new(
                        index.syntax.span.clone(),
                        "compile-time indexing requires a product",
                    ));
                    return None;
                };
                let Value::Integer(position) =
                    self.eval_expression(module, &index.index, environment)?
                else {
                    self.diagnostics.push(Diagnostic::new(
                        index.index.syntax().span.clone(),
                        "compile-time product index must be an integer",
                    ));
                    return None;
                };
                usize::try_from(position)
                    .ok()
                    .and_then(|position| elements.get(position).map(|(_, value)| value.clone()))
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
            Expression::Loop(loop_) => {
                self.diagnostics.push(Diagnostic::new(
                    loop_.syntax.span.clone(),
                    "loops are not supported during compile-time evaluation",
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
                        Statement::Assignment(assignment) => {
                            self.diagnostics.push(Diagnostic::new(
                                assignment.syntax.span.clone(),
                                "mutation is not allowed during compile-time evaluation",
                            ));
                            return None;
                        }
                        Statement::Expression(value) => {
                            result = self.eval_expression(module, value, &mut local)?
                        }
                        Statement::Return(return_) => {
                            return self.eval_expression(module, &return_.value, &mut local);
                        }
                        Statement::Break(break_) => {
                            self.diagnostics.push(Diagnostic::new(
                                break_.syntax.span.clone(),
                                "`break` is not supported during compile-time evaluation",
                            ));
                            return None;
                        }
                        Statement::Continue(continue_) => {
                            self.diagnostics.push(Diagnostic::new(
                                continue_.syntax.span.clone(),
                                "`continue` is not supported during compile-time evaluation",
                            ));
                            return None;
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
                self.freshen_expression(&mut function.body, module, mark);
            }
            Expression::Satisfies(satisfies) => {
                self.freshen_expression(&mut satisfies.value, module, mark);
                freshen_type(self, &mut satisfies.ty, module, mark);
            }
            Expression::Match(match_) => {
                self.freshen_expression(&mut match_.subject, module, mark);
                for arm in &mut match_.arms {
                    self.freshen_syntax(&mut arm.syntax, module, mark);
                    freshen_pattern(self, &mut arm.pattern, module, mark);
                    self.freshen_expression(&mut arm.body, module, mark);
                }
            }
            Expression::Loop(loop_) => {
                self.freshen_syntax(&mut loop_.body.syntax, module, mark);
                for statement in &mut loop_.body.statements {
                    freshen_statement(self, statement, module, mark);
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
            Expression::Index(index) => {
                self.freshen_expression(&mut index.value, module, mark);
                self.freshen_expression(&mut index.index, module, mark);
            }
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
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }
}

fn macro_is_ancestor(program: &Program, ancestor: ModuleId, mut module: ModuleId) -> bool {
    while let Some(parent) = program.parent_module(module) {
        if parent == ancestor {
            return true;
        }
        module = parent;
    }
    false
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

fn meta_type(ty: &Type) -> Option<MetaType> {
    match ty {
        Type::Named(named) if named.namespace.is_none() => match named.name.as_str() {
            "Syntax" => Some(MetaType::Syntax),
            "Expr" => Some(MetaType::Expr),
            "Ident" => Some(MetaType::Ident(None)),
            "Type" => Some(MetaType::Type),
            "Pattern" => Some(MetaType::Pattern),
            "Item" => Some(MetaType::Item),
            _ => None,
        },
        Type::Application(application) => {
            let Type::Named(callee) = application.callee.as_ref() else {
                return None;
            };
            if callee.namespace.is_some() || callee.name != "Ident" {
                return None;
            }
            match application.argument.as_ref() {
                Type::Named(argument)
                    if argument.namespace.is_none() && argument.name == "String" =>
                {
                    Some(MetaType::Ident(None))
                }
                Type::StringLiteral(literal) => crate::string_literal::decode(&literal.literal)
                    .ok()
                    .map(|spelling| MetaType::Ident(Some(spelling))),
                _ => None,
            }
        }
        _ => None,
    }
}

fn meta_type_matches(expected: &MetaType, argument: &Expression) -> bool {
    match expected {
        MetaType::Syntax | MetaType::Expr => true,
        MetaType::Ident(spelling) => match argument {
            Expression::Name(name) if is_plain_identifier(name) => spelling
                .as_ref()
                .is_none_or(|expected| expected == &name.name),
            _ => false,
        },
        MetaType::Type | MetaType::Pattern | MetaType::Item => false,
    }
}

fn meta_type_at_least_as_specific(left: &MetaType, right: &MetaType) -> bool {
    left == right
        || matches!(right, MetaType::Syntax)
        || matches!(
            (left, right),
            (MetaType::Ident(_), MetaType::Expr)
                | (MetaType::Ident(Some(_)), MetaType::Ident(None))
        )
}

fn signature_more_specific(left: &[MetaType], right: &[MetaType]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| meta_type_at_least_as_specific(left, right))
        && left != right
}

fn format_meta_signature(parameters: &[MetaType]) -> String {
    parameters
        .iter()
        .map(|parameter| match parameter {
            MetaType::Syntax => "Syntax".to_owned(),
            MetaType::Expr => "Expr".to_owned(),
            MetaType::Ident(None) => "Ident String".to_owned(),
            MetaType::Ident(Some(spelling)) => format!("Ident {spelling:?}"),
            MetaType::Type => "Type".to_owned(),
            MetaType::Pattern => "Pattern".to_owned(),
            MetaType::Item => "Item".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn macro_signature(annotation: &Type) -> Option<(Vec<MetaType>, MetaType)> {
    let mut parameters = Vec::new();
    let mut current = annotation;
    while let Type::Function(function) = current {
        parameters.push(meta_type(&function.parameter)?);
        current = &function.result;
    }
    let result = meta_type(current)?;
    (!parameters.is_empty()).then_some((parameters, result))
}

fn inferred_macro_signature(value: Option<&Expression>) -> (Vec<MetaType>, MetaType) {
    let mut parameters = Vec::new();
    let mut current = value;
    while let Some(Expression::Function(function)) = current {
        parameters.push(pattern_meta_type(&function.pattern).unwrap_or(MetaType::Syntax));
        current = Some(&function.body);
    }
    (parameters, MetaType::Syntax)
}

fn macro_body_parameter_types(expression: &Expression) -> Vec<Option<MetaType>> {
    let mut parameters = Vec::new();
    let mut current = expression;
    while let Expression::Function(function) = current {
        parameters.push(match &function.pattern {
            Pattern::Binding(binding) if matches!(binding.ty, Type::Inferred(_)) => None,
            pattern => pattern_meta_type(pattern),
        });
        current = &function.body;
    }
    parameters
}

fn pattern_meta_type(pattern: &Pattern) -> Option<MetaType> {
    match pattern {
        Pattern::Binding(binding) => match &binding.ty {
            Type::Inferred(_) => Some(MetaType::Syntax),
            ty => meta_type(ty),
        },
        Pattern::Wildcard(_) => Some(MetaType::Syntax),
        Pattern::Nominal(pattern) if pattern.namespace.is_none() && pattern.name == "Ident" => {
            let Pattern::StringLiteral(literal) = pattern.argument.as_ref() else {
                return None;
            };
            crate::string_literal::decode(&literal.literal)
                .ok()
                .map(|spelling| MetaType::Ident(Some(spelling)))
        }
        Pattern::Product(_) | Pattern::Nominal(_) | Pattern::StringLiteral(_) => None,
    }
}

fn type_contains_syntax(ty: &Type) -> bool {
    match ty {
        Type::Named(named) => matches!(
            named.name.as_str(),
            "Syntax" | "Expr" | "Ident" | "Type" | "Pattern" | "Item"
        ),
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
        Type::Repeated(repeated) => type_contains_syntax(&repeated.element),
        Type::Inferred(_) | Type::StringLiteral(_) => false,
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
    if let Expression::Satisfies(satisfies) = expression {
        return expression_parameter_contains_syntax(&satisfies.value);
    }
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
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) => false,
    }
}

fn obviously_not_syntax(expression: &Expression, arity: usize) -> bool {
    if arity > 0 {
        return match expression {
            Expression::Function(function) => obviously_not_syntax(&function.body, arity - 1),
            Expression::Satisfies(satisfies) => obviously_not_syntax(&satisfies.value, arity),
            _ => true,
        };
    }
    match expression {
        Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::Name(_)
        | Expression::Call(_)
        | Expression::Access(_)
        | Expression::Index(_) => false,
        Expression::Satisfies(satisfies) => obviously_not_syntax(&satisfies.value, 0),
        Expression::Match(match_) => match_
            .arms
            .iter()
            .any(|arm| obviously_not_syntax(&arm.body, 0)),
        Expression::Loop(_) => true,
        Expression::Block(block) => {
            block
                .statements
                .last()
                .is_none_or(|statement| match statement {
                    Statement::Expression(expression) => obviously_not_syntax(expression, 0),
                    Statement::Return(return_) => obviously_not_syntax(&return_.value, 0),
                    Statement::Binding(_)
                    | Statement::PatternBinding(_)
                    | Statement::Assignment(_)
                    | Statement::Break(_)
                    | Statement::Continue(_) => true,
                })
        }
        Expression::Infix(_)
        | Expression::Function(_)
        | Expression::Product(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => true,
    }
}

fn valid_macro_annotation(annotation: &Type) -> bool {
    macro_signature(annotation).is_some()
}

fn valid_macro_parameter_patterns(expression: &Expression, arity: usize) -> bool {
    if arity == 0 {
        return true;
    }
    let Expression::Function(function) = expression else {
        return false;
    };
    let valid_pattern = pattern_meta_type(&function.pattern).is_some();
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

fn is_plain_identifier(name: &crate::NameExpression) -> bool {
    let mut tokens = name
        .syntax
        .tokens()
        .iter()
        .filter(|token| !token.kind.is_trivia());
    matches!(tokens.next(), Some(token) if token.kind == crate::TokenKind::Identifier)
        && tokens.next().is_none()
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
        Pattern::StringLiteral(pattern) => {
            let Value::String(value) = value else {
                return false;
            };
            crate::string_literal::decode(&pattern.literal).is_ok_and(|literal| literal == value)
        }
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
            if pattern.namespace.is_none()
                && pattern.name == "Ident"
                && let Value::Syntax(Expression::Name(name)) = &value
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::String(name.name.clone()),
                    environment,
                );
            }
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
        Expression::Satisfies(satisfies) => {
            *satisfies.value = substitute_splices(&satisfies.value, environment, diagnostics)?
        }
        Expression::Match(match_) => {
            *match_.subject = substitute_splices(&match_.subject, environment, diagnostics)?;
            for arm in &mut match_.arms {
                arm.body = substitute_splices(&arm.body, environment, diagnostics)?;
            }
        }
        Expression::Loop(loop_) => {
            for statement in &mut loop_.body.statements {
                substitute_statement(statement, environment, diagnostics)?;
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
        Expression::Index(index) => {
            *index.value = substitute_splices(&index.value, environment, diagnostics)?;
            *index.index = substitute_splices(&index.index, environment, diagnostics)?;
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
        | Expression::Integer(_)
        | Expression::Float(_) => {}
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
        Statement::Assignment(assignment) => {
            assignment.target = substitute_splices(&assignment.target, environment, diagnostics)?;
            assignment.value = substitute_splices(&assignment.value, environment, diagnostics)?;
        }
        Statement::Return(return_) => {
            return_.value = substitute_splices(&return_.value, environment, diagnostics)?
        }
        Statement::Break(break_) => {
            if let Some(value) = &mut break_.value {
                *value = substitute_splices(value, environment, diagnostics)?;
            }
        }
        Statement::Continue(_) => {}
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
            binding
                .resolution_name
                .get_or_insert_with(|| binding.name.clone());
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
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
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
        Expression::Satisfies(satisfies) => {
            alpha_rename_expression(&mut satisfies.value, mark, scopes)
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
        Expression::Loop(loop_) => {
            alpha_rename_block(&mut loop_.body, mark, scopes);
        }
        Expression::Block(block) => {
            alpha_rename_block(block, mark, scopes);
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
        Expression::Index(index) => {
            alpha_rename_expression(&mut index.value, mark, scopes);
            alpha_rename_expression(&mut index.index, mark, scopes);
        }
        Expression::Infix(infix) => {
            for operand in &mut infix.operands {
                alpha_rename_expression(operand, mark, scopes);
            }
        }
        Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
    }
}

fn alpha_rename_block(
    block: &mut BlockExpression,
    mark: u64,
    scopes: &mut Vec<HashMap<String, String>>,
) {
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
                alpha_rename_pattern(&mut binding.pattern, mark, scopes.last_mut().unwrap());
            }
            Statement::Assignment(assignment) => {
                alpha_rename_expression(&mut assignment.target, mark, scopes);
                alpha_rename_expression(&mut assignment.value, mark, scopes);
            }
            Statement::Return(return_) => alpha_rename_expression(&mut return_.value, mark, scopes),
            Statement::Break(break_) => {
                if let Some(value) = &mut break_.value {
                    alpha_rename_expression(value, mark, scopes);
                }
            }
            Statement::Continue(_) => {}
            Statement::Expression(expression) => alpha_rename_expression(expression, mark, scopes),
        }
    }
    scopes.pop();
}

fn expression_syntax_mut(expression: &mut Expression) -> &mut Syntax {
    match expression {
        Expression::Function(value) => &mut value.syntax,
        Expression::Satisfies(value) => &mut value.syntax,
        Expression::Match(value) => &mut value.syntax,
        Expression::Loop(value) => &mut value.syntax,
        Expression::Block(value) => &mut value.syntax,
        Expression::Product(value) => &mut value.syntax,
        Expression::Call(value) => &mut value.syntax,
        Expression::Access(value) => &mut value.syntax,
        Expression::Index(value) => &mut value.syntax,
        Expression::Infix(value) => &mut value.syntax,
        Expression::Quote(value) => &mut value.syntax,
        Expression::Splice(value) => &mut value.syntax,
        Expression::Name(value) => &mut value.syntax,
        Expression::String(value) => &mut value.syntax,
        Expression::CString(value) => &mut value.syntax,
        Expression::Integer(value) => &mut value.syntax,
        Expression::Float(value) => &mut value.syntax,
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
        Pattern::StringLiteral(literal) => {
            expander.freshen_syntax(&mut literal.syntax, module, mark)
        }
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
        Statement::Assignment(assignment) => {
            expander.freshen_syntax(&mut assignment.syntax, module, mark);
            expander.freshen_expression(&mut assignment.target, module, mark);
            expander.freshen_expression(&mut assignment.value, module, mark);
        }
        Statement::Return(return_) => {
            expander.freshen_syntax(&mut return_.syntax, module, mark);
            expander.freshen_expression(&mut return_.value, module, mark);
        }
        Statement::Break(break_) => {
            expander.freshen_syntax(&mut break_.syntax, module, mark);
            if let Some(value) = &mut break_.value {
                expander.freshen_expression(value, module, mark);
            }
        }
        Statement::Continue(continue_) => {
            expander.freshen_syntax(&mut continue_.syntax, module, mark);
        }
        Statement::Expression(expression) => expander.freshen_expression(expression, module, mark),
    }
}

fn freshen_type(expander: &mut MacroExpander, ty: &mut Type, module: ModuleId, mark: u64) {
    let syntax = match ty {
        Type::Inferred(ty) => &mut ty.syntax,
        Type::StringLiteral(ty) => &mut ty.syntax,
        Type::Named(ty) => &mut ty.syntax,
        Type::Product(ty) => &mut ty.syntax,
        Type::Sum(ty) => &mut ty.syntax,
        Type::Function(ty) => &mut ty.syntax,
        Type::Application(ty) => &mut ty.syntax,
        Type::Repeated(ty) => &mut ty.syntax,
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
        Type::Repeated(repeated) => freshen_type(expander, &mut repeated.element, module, mark),
        Type::Inferred(_) | Type::StringLiteral(_) | Type::Named(_) => {}
    }
}
