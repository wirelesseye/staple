use std::{cell::RefCell, collections::HashMap, rc::Rc};

use crate::{
    Accessor, Binding, BindingKind, BlockExpression, Diagnostic, Expression, Item,
    MacroDeclaration, ModifierArgument, ModifierInvocation, ModuleId, Pattern, Program,
    ResolvedMacro, Span, Statement, Syntax, SyntaxId, Type, UseKind, Visibility, VisibilityKind,
    VisibilitySyntax,
};

const MAX_EXPANSION_DEPTH: usize = 128;
const MAX_EVALUATION_STEPS: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MacroKey {
    module: ModuleId,
    name: String,
    modifier: bool,
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

#[derive(Clone)]
enum MacroArgument {
    Expression(Expression),
    Visibility(VisibilitySyntax),
}

#[derive(Clone)]
struct SelectedMacro {
    definition: MacroDefinition,
    arguments: Vec<MacroArgument>,
    consumed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MetaType {
    Syntax,
    Expr,
    Ident(Option<String>),
    CallExpr,
    UnstructuredExpr,
    Type,
    Pattern,
    Item,
    Visibility,
    MacroCallVisibility,
    Comma,
    Equals,
    FatArrow,
    Product(Vec<MetaType>),
    Optional(Box<MetaType>),
    Sequence(Box<MetaType>),
    Delimited(DelimiterKind, DelimitedMetaContents),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DelimiterKind {
    Parenthesized,
    Bracketed,
    Braced,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DelimitedMetaContents {
    Fixed(Vec<MetaType>),
    Sequence(Box<MetaType>),
    Separated {
        element: Box<MetaType>,
        separator: Box<MetaType>,
    },
}

impl MetaType {
    fn is_expression(&self) -> bool {
        matches!(
            self,
            Self::Expr
                | Self::Ident(_)
                | Self::CallExpr
                | Self::UnstructuredExpr
                | Self::Delimited(DelimiterKind::Parenthesized | DelimiterKind::Braced, _)
        )
    }
}

#[derive(Clone, Default)]
struct ModuleScope {
    macros: HashMap<String, Vec<MacroKey>>,
    modifiers: HashMap<String, Vec<MacroKey>>,
    namespaces: HashMap<String, ModuleId>,
    helpers: HashMap<String, HelperDefinition>,
}

#[derive(Clone)]
struct HelperDefinition {
    module: ModuleId,
    binding: Binding,
}

#[derive(Clone)]
enum SyntaxValue {
    Ident(crate::NameExpression),
    Call(crate::CallExpression),
    Unstructured(Expression),
    Type(Type),
    Pattern(Pattern),
    Item(Box<Item>),
    Items(Vec<Item>),
    Visibility(VisibilitySyntax),
    Comma(Syntax),
    Equals(Syntax),
    FatArrow(Syntax),
    Delimited(DelimitedSyntaxValue),
}

#[derive(Clone)]
struct DelimitedSyntaxValue {
    kind: DelimiterKind,
    syntax: Syntax,
    contents: DelimitedValueContents,
    generated: Vec<Syntax>,
    expression: Option<Box<Expression>>,
}

#[derive(Clone)]
enum DelimitedValueContents {
    Fixed(Vec<(Option<String>, Value)>),
    Sequence(Vec<Value>),
    Separated {
        elements: Vec<Value>,
        separator: Box<Value>,
        trailing: bool,
    },
}

impl SyntaxValue {
    fn from_expression(expression: Expression) -> Self {
        match expression {
            Expression::Name(name) => Self::Ident(name),
            Expression::Call(call) => Self::Call(call),
            Expression::VisibilityArgument(visibility) => Self::Visibility(visibility),
            expression => Self::Unstructured(expression),
        }
    }

    fn to_expression(&self) -> Option<Expression> {
        self.clone().into_expression()
    }

    fn into_expression(self) -> Option<Expression> {
        match self {
            Self::Ident(name) => Some(Expression::Name(name)),
            Self::Call(call) => Some(Expression::Call(call)),
            Self::Unstructured(expression) => Some(expression),
            Self::Delimited(delimited) => delimited.into_expression(),
            Self::Type(_)
            | Self::Pattern(_)
            | Self::Item(_)
            | Self::Items(_)
            | Self::Visibility(_)
            | Self::Comma(_)
            | Self::Equals(_)
            | Self::FatArrow(_) => None,
        }
    }
}

impl DelimitedSyntaxValue {
    fn into_expression(self) -> Option<Expression> {
        if let Some(expression) = self.expression {
            return Some(*expression);
        }
        let mut generated = self.generated.into_iter();
        match self.kind {
            DelimiterKind::Bracketed => None,
            DelimiterKind::Parenthesized => {
                let values = match self.contents {
                    DelimitedValueContents::Fixed(values) => values,
                    DelimitedValueContents::Sequence(values) => {
                        values.into_iter().map(|value| (None, value)).collect()
                    }
                    DelimitedValueContents::Separated { elements, .. } => {
                        return separated_parenthesized(self.syntax, elements, &mut generated);
                    }
                };
                let mut expressions = values
                    .into_iter()
                    .map(|(_, value)| match value {
                        Value::Syntax(value) => value.into_expression(),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                if expressions.is_empty() {
                    return Some(Expression::Product(crate::ProductExpression {
                        syntax: self.syntax,
                        elements: Vec::new(),
                    }));
                }
                let first = expressions.remove(0);
                let expression = expressions.into_iter().fold(first, |callee, argument| {
                    Expression::Call(crate::CallExpression {
                        syntax: generated.next().unwrap_or_else(Syntax::compiler),
                        callee: Box::new(callee),
                        argument: Box::new(argument),
                    })
                });
                Some(Expression::Product(crate::ProductExpression {
                    syntax: self.syntax,
                    elements: vec![crate::ProductElement {
                        syntax: generated.next().unwrap_or_else(Syntax::compiler),
                        name: None,
                        value: expression,
                        spread: false,
                    }],
                }))
            }
            DelimiterKind::Braced => {
                let values = match self.contents {
                    DelimitedValueContents::Fixed(values) => values
                        .into_iter()
                        .map(|(_, value)| value)
                        .collect::<Vec<_>>(),
                    DelimitedValueContents::Sequence(values) => values,
                    DelimitedValueContents::Separated { .. } => return None,
                };
                let statements = values
                    .into_iter()
                    .map(|value| match value {
                        Value::Syntax(value) => value.into_expression().map(Statement::Expression),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(Expression::Block(BlockExpression {
                    syntax: self.syntax,
                    statements,
                }))
            }
        }
    }
}

fn syntax_category(value: &SyntaxValue) -> &'static str {
    match value {
        SyntaxValue::Ident(_) | SyntaxValue::Call(_) | SyntaxValue::Unstructured(_) => "expression",
        SyntaxValue::Type(_) => "type",
        SyntaxValue::Pattern(_) => "pattern",
        SyntaxValue::Item(_) => "item",
        SyntaxValue::Items(_) => "item sequence",
        SyntaxValue::Visibility(_) => "visibility",
        SyntaxValue::Comma(syntax) => {
            let _ = syntax.id;
            "comma"
        }
        SyntaxValue::Equals(syntax) => {
            let _ = syntax.id;
            "equals"
        }
        SyntaxValue::FatArrow(syntax) => {
            let _ = syntax.id;
            "fat arrow"
        }
        SyntaxValue::Delimited(value) => match value.kind {
            DelimiterKind::Parenthesized => "parenthesized",
            DelimiterKind::Bracketed => "bracketed",
            DelimiterKind::Braced => "braced",
        },
    }
}

#[derive(Clone)]
enum Value {
    Syntax(SyntaxValue),
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
    Sequence(Vec<Value>),
    Separated {
        elements: Vec<Value>,
        separator: Box<Value>,
        trailing: bool,
    },
}

fn separated_parenthesized(
    syntax: Syntax,
    elements: Vec<Value>,
    generated: &mut impl Iterator<Item = Syntax>,
) -> Option<Expression> {
    let elements = elements
        .into_iter()
        .map(|value| match value {
            Value::Syntax(value) => Some(crate::ProductElement {
                syntax: generated.next().unwrap_or_else(Syntax::compiler),
                name: None,
                value: value.into_expression()?,
                spread: false,
            }),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Expression::Product(crate::ProductExpression {
        syntax,
        elements,
    }))
}

#[derive(Clone)]
struct EnvironmentBinding {
    value: Rc<RefCell<Value>>,
    mutable: bool,
}

impl EnvironmentBinding {
    fn new(value: Value, mutable: bool) -> Self {
        Self {
            value: Rc::new(RefCell::new(value)),
            mutable,
        }
    }

    fn get(&self) -> Value {
        self.value.borrow().clone()
    }
}

type Environment = HashMap<String, EnvironmentBinding>;

pub(crate) struct MacroAnalysis {
    pub definitions: HashMap<SyntaxId, ResolvedMacro>,
    pub invocations: HashMap<SyntaxId, ResolvedMacro>,
}

pub(crate) fn expand_program(
    mut program: Program,
) -> Result<(Program, MacroAnalysis), Vec<Diagnostic>> {
    let mut expander = MacroExpander::new(&program);
    expander.validate_definitions();
    for module in program.modules() {
        for item in &module.syntax.items {
            if let Item::Statement(statement) = item
                && let Statement::Binding(binding) = statement.as_ref()
                && binding.kind == BindingKind::Def
                && binding_uses_macro_call_visibility(binding)
            {
                expander.diagnostics.push(Diagnostic::new(
                    binding.syntax.span.clone(),
                    "`MacroCallVisibility` may only be the first parameter of a function-style macro",
                ));
            }
        }
    }
    if !expander.diagnostics.is_empty() {
        return Err(expander.diagnostics);
    }

    for source_module in program.modules_mut() {
        let module = source_module.id;
        let mut items = Vec::new();
        for mut item in source_module.syntax.items.clone() {
            expander.expand_item(module, &mut item, 0);
            if let Some(generated) = expander.emitted_items.take() {
                items.extend(expander.expand_generated_items(module, generated, 1));
            } else {
                items.push(item);
            }
        }
        items.retain(|item| {
            !matches!(item,
                Item::Statement(statement)
                    if matches!(statement.as_ref(), Statement::Binding(binding)
                        if binding_is_compile_time_helper(binding))
            )
        });
        let declared = items
            .iter()
            .filter_map(|item| match item {
                Item::TypeDeclaration(declaration) => Some(declaration.name.as_str()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        for item in &items {
            if let Item::Statement(statement) = item
                && let Statement::Binding(binding) = statement.as_ref()
                && binding.kind != BindingKind::Def
                && binding
                    .annotation
                    .as_ref()
                    .is_some_and(|ty| type_contains_unshadowed_syntax(ty, &declared))
            {
                expander.diagnostics.push(Diagnostic::new(
                    binding.syntax.span.clone(),
                    "`Syntax` values are compile-time-only",
                ));
            }
        }
        source_module.syntax.items = items;
    }
    program.rebuild_generated_inline_modules();
    for source_module in program.modules() {
        for item in &source_module.syntax.items {
            if let Item::UseDeclaration(declaration) = item
                && program.imported_module(declaration.syntax.id).is_none()
            {
                expander.diagnostics.push(Diagnostic::new(
                    declaration.syntax.span.clone(),
                    format!("could not resolve module `{}`", declaration.path.join(".")),
                ));
            }
        }
    }
    if expander.diagnostics.is_empty() {
        Ok((program, expander.analysis()))
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
    emitted_items: Option<Vec<Item>>,
    invocations: HashMap<SyntaxId, ResolvedMacro>,
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
                            modifier: declaration.modifier,
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
                        let (mut parameters, mut result) = declaration
                            .annotation
                            .as_ref()
                            .and_then(macro_signature)
                            .unwrap_or_else(|| {
                                inferred_macro_signature(declaration.value.as_ref())
                            });
                        if declaration.modifier && declaration.annotation.is_none() {
                            if parameters.len() == 2 && parameters[0] == MetaType::Syntax {
                                parameters[0] = MetaType::Expr;
                            }
                            if let Some(last) = parameters.last_mut() {
                                *last = MetaType::Item;
                            }
                            result = MetaType::Item;
                        }
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
                        let namespace = if declaration.modifier {
                            &mut scopes[source_module.id.0].modifiers
                        } else {
                            &mut scopes[source_module.id.0].macros
                        };
                        namespace
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

        let mut public_macros = HashMap::<(ModuleId, String, bool), Vec<MacroKey>>::new();
        for definition in definitions
            .values()
            .filter(|definition| definition.declaration.visibility == Visibility::Public)
        {
            public_macros
                .entry((
                    definition.key.module,
                    definition.key.name.clone(),
                    definition.key.modifier,
                ))
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
                            .filter(|(module, _, _)| *module == imported)
                            .map(|(_, name, _)| (name.clone(), name.clone()))
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
                        for modifier in [false, true] {
                            if let Some(keys) =
                                previous_macros.get(&(imported, item.clone(), modifier))
                            {
                                changed |= public_macros
                                    .insert(
                                        (source_module.id, alias.clone(), modifier),
                                        keys.clone(),
                                    )
                                    .is_none();
                            }
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

        let mut all_macros = HashMap::<(ModuleId, String, bool), Vec<MacroKey>>::new();
        for definition in definitions.values() {
            all_macros
                .entry((
                    definition.key.module,
                    definition.key.name.clone(),
                    definition.key.modifier,
                ))
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
                for ((module, name, modifier), keys) in &public_macros {
                    if *module != core {
                        continue;
                    }
                    let namespace = if *modifier {
                        &mut scopes[source_module.id.0].modifiers
                    } else {
                        &mut scopes[source_module.id.0].macros
                    };
                    namespace
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
            for (namespace, target) in program.root_qualified_modules(source_module.id) {
                scopes[source_module.id.0]
                    .namespaces
                    .insert(namespace.to_owned(), target);
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
                        for ((_, name, modifier), keys) in macros
                            .iter()
                            .filter(|((module, _, _), _)| *module == imported)
                        {
                            let namespace = if *modifier {
                                &mut scopes[source_module.id.0].modifiers
                            } else {
                                &mut scopes[source_module.id.0].macros
                            };
                            namespace
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
            emitted_items: None,
            invocations: HashMap::new(),
        }
    }

    fn analysis(&self) -> MacroAnalysis {
        MacroAnalysis {
            definitions: self
                .definitions
                .values()
                .map(|definition| (definition.declaration.syntax.id, resolved_macro(definition)))
                .collect(),
            invocations: self.invocations.clone(),
        }
    }

    fn record_invocation(&mut self, syntax: SyntaxId, definition: &MacroDefinition) {
        self.invocations.insert(syntax, resolved_macro(definition));
    }

    fn install_selected(
        scope: &mut ModuleScope,
        imported: ModuleId,
        item: &str,
        local: &str,
        public_macros: &HashMap<(ModuleId, String, bool), Vec<MacroKey>>,
        public_helpers: &HashMap<(ModuleId, String), HelperDefinition>,
    ) {
        for modifier in [false, true] {
            if let Some(keys) = public_macros.get(&(imported, item.to_owned(), modifier)) {
                let namespace = if modifier {
                    &mut scope.modifiers
                } else {
                    &mut scope.macros
                };
                namespace
                    .entry(local.to_owned())
                    .or_insert_with(|| keys.clone());
            }
        }
        if let Some(helper) = public_helpers.get(&(imported, item.to_owned())) {
            scope
                .helpers
                .entry(local.to_owned())
                .or_insert_with(|| helper.clone());
        }
    }

    fn validate_definitions(&mut self) {
        let mut groups = HashMap::<(ModuleId, String, bool), Vec<MacroDefinition>>::new();
        for definition in self.definitions.values() {
            groups
                .entry((
                    definition.key.module,
                    definition.key.name.clone(),
                    definition.key.modifier,
                ))
                .or_default()
                .push(definition.clone());
        }
        for ((_, name, modifier), mut definitions) in groups {
            definitions.sort_by_key(|definition| definition.key.syntax.0);
            for (index, definition) in definitions.iter().enumerate() {
                if let Some(previous) = definitions[..index]
                    .iter()
                    .find(|previous| previous.parameters == definition.parameters)
                {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "duplicate {}macro overload `{}{name}: {}`",
                            if modifier { "modifier " } else { "" },
                            if modifier { "@" } else { "" },
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
            if definition.declaration.annotation.is_none()
                && definition
                    .declaration
                    .value
                    .as_ref()
                    .is_some_and(invalid_delimited_collection_parameter)
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "`Sequence` and `Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`",
                ));
            }
            if let Some(annotation) = &definition.declaration.annotation
                && !valid_macro_annotation(annotation)
            {
                self.diagnostics.push(Diagnostic::new(
                    annotation.syntax().span.clone(),
                    if type_contains_named(annotation, "Sequence")
                        || type_contains_named(annotation, "Separated")
                    {
                        "`Sequence` and `Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`"
                    } else {
                        "a macro annotation must accept one or more syntax-category parameters and return a syntax category"
                    },
                ));
            }
            if definition
                .parameters
                .iter()
                .any(|parameter| matches!(parameter, MetaType::Optional(_) | MetaType::Product(_)))
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "`Optional` and product syntax shapes may only appear inside delimited contents",
                ));
            }
            if definition
                .parameters
                .iter()
                .any(|parameter| matches!(parameter, MetaType::Sequence(_)))
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "`Sequence` and `Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`",
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
                    let implicit_modifier_item = definition.key.modifier
                        && index + 1 == parameters.len()
                        && *declared == MetaType::Item
                        && body_parameter == Some(MetaType::Syntax);
                    if !implicit_modifier_item
                        && body_parameter.is_some_and(|body_parameter| body_parameter != *declared)
                    {
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
            if definition.key.modifier {
                if definition.key.name == "recursive_constructor" {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        "modifier name `@recursive_constructor` is reserved by the compiler",
                    ));
                }
                let valid_parameters = match definition.parameters.as_slice() {
                    [MetaType::Item] => true,
                    [argument, MetaType::Item] => matches!(
                        argument,
                        MetaType::Expr
                            | MetaType::Ident(_)
                            | MetaType::CallExpr
                            | MetaType::UnstructuredExpr
                            | MetaType::Type
                            | MetaType::Pattern
                    ),
                    _ => false,
                };
                if !valid_parameters || definition.result != MetaType::Item {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "modifier macro `@{}` must have signature `Item -> Item` or `(Expr | Type | Pattern) -> Item -> Item`",
                            definition.key.name
                        ),
                    ));
                }
            } else if definition.parameters.contains(&MetaType::Item) {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    format!(
                        "function-style macro `{}` cannot accept `Item`; define a modifier macro with `macro @{}` instead",
                        definition.key.name, definition.key.name
                    ),
                ));
            }
            if definition.key.modifier
                && definition.parameters.iter().any(|parameter| {
                    matches!(
                        parameter,
                        MetaType::Visibility | MetaType::MacroCallVisibility
                    )
                })
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "modifier macros do not accept visibility parameters",
                ));
            }
            if !definition.key.modifier
                && (definition
                    .parameters
                    .iter()
                    .enumerate()
                    .any(|(index, parameter)| {
                        *parameter == MetaType::MacroCallVisibility && index != 0
                    })
                    || definition.result == MetaType::MacroCallVisibility)
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    format!(
                        "macro `{}` may use `MacroCallVisibility` only as its first parameter",
                        definition.key.name
                    ),
                ));
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
                MacroKind::User(body) if directly_evaluates_resource(body, definition.arity) => {
                    self.diagnostics.push(Diagnostic::new(
                        body.syntax().span.clone(),
                        "resources are not available during compile-time macro evaluation",
                    ));
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

    fn expand_item(&mut self, module: ModuleId, item: &mut Item, depth: usize) {
        if let Item::VisibilityMacroInvocation(invocation) = item {
            let invocation = invocation.clone();
            if let Some(expanded) =
                self.expand_visibility_macro_invocation(module, invocation, depth)
            {
                *item = expanded;
                self.expand_item(module, item, depth + 1);
            }
            return;
        }
        if let Item::VisibilitySplice(splice) = item {
            self.diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "visibility splices are only available while evaluating `quote`",
            ));
            return;
        }
        if let Item::RepeatedItemSplice(splice) = item {
            self.diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "repeated item splices are only available while evaluating `quote`",
            ));
            return;
        }
        if let Item::Modified(modified) = item {
            let modified = modified.clone();
            if depth == 0 {
                self.steps = 0;
            }
            if let Some(expanded) = self.apply_modifier_chain(module, modified, depth) {
                *item = expanded;
                self.expand_item(module, item, depth + 1);
            }
            return;
        }
        if let Item::Statement(statement) = item
            && let Statement::Expression(expression) = statement.as_ref()
        {
            let expression = expression.clone();
            if self.expand_top_level_macro(module, item, expression, depth) {
                return;
            }
        }
        match item {
            Item::Statement(statement)
                if matches!(statement.as_ref(), Statement::Binding(binding)
                    if binding_is_compile_time_helper(binding)) => {}
            Item::Statement(statement) => self.expand_statement(module, statement, depth),
            Item::TraitImplementation(implementation) => {
                for member in &mut implementation.members {
                    member.value = self.expand_expression(module, member.value.clone(), depth);
                }
            }
            Item::TraitDeclaration(declaration) => {
                for member in &mut declaration.members {
                    if let Some(default) = member.default.take() {
                        member.default = Some(self.expand_expression(module, default, depth));
                    }
                }
            }
            Item::Submodule(submodule) if submodule.syntax.definition_module().is_some() => {
                let items = std::mem::take(&mut submodule.module.items);
                submodule.module.items = self.expand_generated_items(module, items, depth + 1);
            }
            Item::MacroDeclaration(_)
            | Item::Modified(_)
            | Item::VisibilityMacroInvocation(_)
            | Item::VisibilitySplice(_)
            | Item::RepeatedItemSplice(_)
            | Item::Submodule(_)
            | Item::UseDeclaration(_)
            | Item::ExternBlock(_)
            | Item::TypeDeclaration(_) => {}
        }
    }

    fn expand_visibility_macro_invocation(
        &mut self,
        module: ModuleId,
        invocation: crate::VisibilityMacroInvocation,
        depth: usize,
    ) -> Option<Item> {
        let (head, arguments) = flatten_call(&invocation.expression);
        let Some(keys) = self.resolve_macro(module, head) else {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "a leading `pub` or `pub(repr)` must prefix a macro whose first parameter is `MacroCallVisibility`",
            ));
            return None;
        };
        let selected = self.select_macro(
            &keys,
            &arguments,
            Some(&invocation.visibility),
            invocation.syntax.span.clone(),
        )?;
        self.record_invocation(head.syntax().id, &selected.definition);
        if depth >= MAX_EXPANSION_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "macro expansion exceeded the limit of 128 nested expansions",
            ));
            return None;
        }
        let definition = selected.definition;
        let consumed_count = selected.consumed;
        let key = definition.key.clone();
        if self.expansion_stack.contains(&key) {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                format!("recursive macro expansion of `{}`", key.name),
            ));
            return None;
        }
        self.expansion_stack.push(key.clone());
        let diagnostic_start = self.diagnostics.len();
        let result = self.invoke_macro(
            &definition,
            selected.arguments,
            invocation.syntax.span.clone(),
        );
        let Some(result) = result else {
            self.expansion_stack.pop();
            if self.diagnostics.len() > diagnostic_start {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    format!("while expanding macro `{}`", key.name),
                ));
            }
            return None;
        };
        let result = match result {
            SyntaxValue::Items(items) => {
                if arguments[consumed_count..].is_empty() {
                    self.emitted_items = Some(items);
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!(
                            "item-producing macro `{}` cannot have excess arguments",
                            key.name
                        ),
                    ));
                }
                None
            }
            SyntaxValue::Item(item) => {
                if arguments[consumed_count..].is_empty() {
                    Some(*item)
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!(
                            "item-producing macro `{}` cannot have excess arguments",
                            key.name
                        ),
                    ));
                    None
                }
            }
            syntax if syntax.to_expression().is_some() => {
                if definition.result == MetaType::Item {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!(
                            "macro `{}` declared `Item` but returned expression syntax",
                            key.name
                        ),
                    ));
                    None
                } else {
                    let mut expression = syntax.into_expression().unwrap();
                    for argument in &arguments[consumed_count..] {
                        let mut syntax = expression.syntax().clone();
                        syntax.id = self.fresh_id();
                        expression = Expression::Call(crate::CallExpression {
                            syntax,
                            callee: Box::new(expression),
                            argument: Box::new((*argument).clone()),
                        });
                    }
                    Some(Item::Statement(Box::new(Statement::Expression(expression))))
                }
            }
            syntax => {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span.clone(),
                    format!(
                        "macro `{}` produces {} syntax, which cannot replace a top-level item",
                        key.name,
                        syntax_category(&syntax)
                    ),
                ));
                None
            }
        };
        self.expansion_stack.pop();
        result
    }

    fn apply_modifier_chain(
        &mut self,
        module: ModuleId,
        modified: crate::ModifiedItem,
        depth: usize,
    ) -> Option<Item> {
        let mut current = *modified.item;
        if let Item::Modified(nested) = current {
            current = self.apply_modifier_chain(module, nested, depth + 1)?;
        }
        if let Item::VisibilityMacroInvocation(invocation) = current {
            current = self.expand_visibility_macro_invocation(module, invocation, depth + 1)?;
        }
        if !modifier_target_supported(&current) {
            self.diagnostics.push(Diagnostic::new(
                modified.syntax.span,
                "modifier macros may only be applied to `let`, `def`, `type`, `extern`, `trait`, or `impl` items",
            ));
            return None;
        }

        for invocation in modified.modifiers.into_iter().rev() {
            if depth >= MAX_EXPANSION_DEPTH {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span.clone(),
                    "macro expansion exceeded the limit of 128 nested expansions",
                ));
                return None;
            }
            if invocation.namespace.is_none() && invocation.name == "recursive_constructor" {
                if invocation.argument.is_some() {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span,
                        "`@recursive_constructor` does not accept an argument",
                    ));
                    return None;
                }
                let Item::TypeDeclaration(declaration) = &mut current else {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span,
                        "`@recursive_constructor` may only modify a type declaration",
                    ));
                    return None;
                };
                declaration.recursive_constructor = true;
                continue;
            }
            let (definition, argument) = self.select_modifier(module, &invocation)?;
            self.record_invocation(invocation.syntax.id, &definition);
            let key = definition.key.clone();
            if self.expansion_stack.contains(&key) {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span.clone(),
                    format!("recursive modifier macro expansion of `@{}`", key.name),
                ));
                return None;
            }
            self.expansion_stack.push(key.clone());
            let diagnostic_start = self.diagnostics.len();
            let result = self.invoke_modifier(
                &definition,
                argument,
                current,
                invocation.syntax.span.clone(),
            );
            let Some(mut result) = result else {
                self.expansion_stack.pop();
                if self.diagnostics.len() > diagnostic_start {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!("while expanding modifier macro `@{}`", key.name),
                    ));
                }
                return None;
            };
            if let Item::Modified(nested) = result {
                let expanded = self.apply_modifier_chain(module, nested, depth + 1);
                self.expansion_stack.pop();
                result = expanded?;
            } else {
                self.expansion_stack.pop();
            }
            if !modifier_target_supported(&result) {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span.clone(),
                    format!(
                        "modifier macro `@{}` produced an unsupported item kind",
                        key.name
                    ),
                ));
                return None;
            }
            current = result;
        }
        Some(current)
    }

    fn resolve_modifier(
        &self,
        module: ModuleId,
        invocation: &ModifierInvocation,
    ) -> Option<Vec<MacroKey>> {
        let context = invocation
            .syntax
            .definition_module()
            .map(ModuleId)
            .unwrap_or(module);
        match &invocation.namespace {
            None => self.scopes[context.0]
                .modifiers
                .get(&invocation.name)
                .cloned(),
            Some(namespace) => {
                let target = self.scopes[context.0].namespaces.get(namespace)?;
                self.scopes[target.0]
                    .modifiers
                    .get(&invocation.name)
                    .map(|keys| {
                        keys.iter()
                            .filter(|key| {
                                self.definitions[*key].declaration.visibility == Visibility::Public
                            })
                            .cloned()
                            .collect()
                    })
            }
        }
    }

    fn select_modifier(
        &mut self,
        module: ModuleId,
        invocation: &ModifierInvocation,
    ) -> Option<(MacroDefinition, Option<SyntaxValue>)> {
        let Some(keys) = self.resolve_modifier(module, invocation) else {
            let context = invocation
                .syntax
                .definition_module()
                .map(ModuleId)
                .unwrap_or(module);
            let normal_exists = invocation.namespace.is_none()
                && self.scopes[context.0].macros.contains_key(&invocation.name);
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                if normal_exists {
                    format!(
                        "macro `{}` is function-style and cannot be used as modifier `@{}`",
                        invocation.name, invocation.name
                    )
                } else {
                    format!("unknown modifier macro `@{}`", invocation.name)
                },
            ));
            return None;
        };
        let definitions = keys
            .iter()
            .filter_map(|key| self.definitions.get(key).cloned())
            .collect::<Vec<_>>();
        let matching = definitions
            .iter()
            .filter(
                |definition| match (&invocation.argument, definition.parameters.as_slice()) {
                    (None, [MetaType::Item]) => true,
                    (Some(argument), [expected, MetaType::Item]) => {
                        modifier_argument_matches(expected, argument)
                    }
                    _ => false,
                },
            )
            .cloned()
            .collect::<Vec<_>>();
        if matching.is_empty() {
            let expects_argument = definitions
                .iter()
                .any(|definition| definition.parameters.len() == 2);
            let accepts_no_argument = definitions
                .iter()
                .any(|definition| definition.parameters == [MetaType::Item]);
            let message = match (&invocation.argument, expects_argument, accepts_no_argument) {
                (None, true, false) => format!(
                    "modifier macro `@{}` requires a parenthesized argument",
                    invocation.name
                ),
                (Some(_), false, true) => format!(
                    "modifier macro `@{}` does not accept an argument",
                    invocation.name
                ),
                _ => format!(
                    "no overload of modifier macro `@{}` matches this invocation",
                    invocation.name
                ),
            };
            self.diagnostics
                .push(Diagnostic::new(invocation.syntax.span.clone(), message));
            return None;
        }
        let undominated = matching
            .iter()
            .filter(|candidate| {
                !matching.iter().any(|other| {
                    other.key != candidate.key
                        && signature_more_specific(&other.parameters, &candidate.parameters)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let [definition] = undominated.as_slice() else {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                format!(
                    "ambiguous invocation of modifier macro `@{}`",
                    invocation.name
                ),
            ));
            return None;
        };
        let argument = match (&invocation.argument, definition.parameters.as_slice()) {
            (Some(argument), [expected, MetaType::Item]) => {
                Some(self.modifier_argument_value(expected, argument)?)
            }
            (None, [MetaType::Item]) => None,
            _ => unreachable!("selected modifier signature must match its invocation"),
        };
        Some((definition.clone(), argument))
    }

    fn modifier_argument_value(
        &mut self,
        expected: &MetaType,
        argument: &ModifierArgument,
    ) -> Option<SyntaxValue> {
        match expected {
            MetaType::Type => {
                crate::parser::parse_type_fragment(&argument.syntax, true, &mut self.next_syntax_id)
                    .ok()
                    .map(SyntaxValue::Type)
            }
            MetaType::Pattern => crate::parser::parse_pattern_fragment(
                &argument.syntax,
                true,
                &mut self.next_syntax_id,
            )
            .ok()
            .map(SyntaxValue::Pattern),
            _ => argument.expression.clone().map(|expression| {
                SyntaxValue::from_expression(
                    meta_argument_expression(expected, &expression).clone(),
                )
            }),
        }
    }

    fn invoke_modifier(
        &mut self,
        definition: &MacroDefinition,
        argument: Option<SyntaxValue>,
        item: Item,
        call_span: Span,
    ) -> Option<Item> {
        let MacroKind::User(body) = &definition.kind else {
            unreachable!("compiler-provided macros cannot be modifiers")
        };
        let mut value =
            self.eval_expression(definition.key.module, body, &mut Environment::new())?;
        if let Some(argument) = argument {
            value = self.apply_value(value, Value::Syntax(argument), call_span.clone())?;
        }
        value = self.apply_value(
            value,
            Value::Syntax(SyntaxValue::Item(Box::new(item))),
            call_span,
        )?;
        match value {
            Value::Syntax(SyntaxValue::Item(item)) => Some(*item),
            Value::Syntax(syntax) => {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    format!(
                        "modifier macro `@{}` must return `Item`, but returned {} syntax",
                        definition.key.name,
                        syntax_category(&syntax)
                    ),
                ));
                None
            }
            _ => {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    format!(
                        "modifier macro `@{}` did not return `Item`",
                        definition.key.name
                    ),
                ));
                None
            }
        }
    }

    fn expand_top_level_macro(
        &mut self,
        module: ModuleId,
        item: &mut Item,
        expression: Expression,
        depth: usize,
    ) -> bool {
        let (head, arguments) = flatten_call(&expression);
        let Some(keys) = self.resolve_macro(module, head) else {
            return false;
        };
        let Some(selected) =
            self.select_macro(&keys, &arguments, None, expression.syntax().span.clone())
        else {
            return true;
        };
        self.record_invocation(head.syntax().id, &selected.definition);
        let definition = selected.definition;
        let consumed_count = selected.consumed;
        if !matches!(
            definition.result,
            MetaType::Syntax | MetaType::Item | MetaType::Sequence(_)
        ) {
            return false;
        }
        if depth >= MAX_EXPANSION_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                expression.syntax().span.clone(),
                "macro expansion exceeded the limit of 128 nested expansions",
            ));
            return true;
        }
        let key = definition.key.clone();
        if self.expansion_stack.contains(&key) && head.syntax().definition_module().is_some() {
            self.diagnostics.push(Diagnostic::new(
                expression.syntax().span.clone(),
                format!("recursive macro expansion of `{}`", key.name),
            ));
            return true;
        }
        if depth == 0 {
            self.steps = 0;
        }
        self.expansion_stack.push(key.clone());
        let diagnostic_start = self.diagnostics.len();
        let result = self.invoke_macro(
            &definition,
            selected.arguments,
            expression.syntax().span.clone(),
        );
        let Some(result) = result else {
            self.expansion_stack.pop();
            if self.diagnostics.len() > diagnostic_start {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    format!("while expanding macro `{}`", key.name),
                ));
            }
            return true;
        };

        match result {
            SyntaxValue::Items(items) => {
                if !arguments[consumed_count..].is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!(
                            "item-producing macro `{}` cannot have excess arguments",
                            key.name
                        ),
                    ));
                } else {
                    self.emitted_items = Some(items);
                }
            }
            SyntaxValue::Item(generated) => {
                if !arguments[consumed_count..].is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!(
                            "item-producing macro `{}` cannot have excess arguments",
                            key.name
                        ),
                    ));
                } else {
                    *item = *generated;
                    self.expand_item(module, item, depth + 1);
                }
            }
            syntax if syntax.to_expression().is_some() => {
                if definition.result == MetaType::Item {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!(
                            "macro `{}` declared `Item` but returned expression syntax",
                            key.name
                        ),
                    ));
                } else {
                    let mut result = syntax.into_expression().unwrap();
                    for argument in &arguments[consumed_count..] {
                        let mut syntax = result.syntax().clone();
                        syntax.id = self.fresh_id();
                        result = Expression::Call(crate::CallExpression {
                            syntax,
                            callee: Box::new(result),
                            argument: Box::new((*argument).clone()),
                        });
                    }
                    result = self.expand_expression(module, result, depth + 1);
                    *item = Item::Statement(Box::new(Statement::Expression(result)));
                }
            }
            syntax => self.diagnostics.push(Diagnostic::new(
                expression.syntax().span.clone(),
                format!(
                    "macro `{}` produces {} syntax, which cannot replace a top-level item",
                    key.name,
                    syntax_category(&syntax)
                ),
            )),
        }
        self.expansion_stack.pop();
        true
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
            let Some(selected) =
                self.select_macro(&keys, &arguments, None, expression.syntax().span.clone())
            else {
                return expression;
            };
            self.record_invocation(head.syntax().id, &selected.definition);
            let definition = selected.definition;
            let consumed_count = selected.consumed;
            let key = definition.key.clone();
            if definition.result == MetaType::Item {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    format!(
                        "macro `{}` produces item syntax and may only be invoked as a standalone top-level item",
                        key.name
                    ),
                ));
                return expression;
            }
            if !matches!(definition.result, MetaType::Syntax) && !definition.result.is_expression()
            {
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
            let diagnostic_start = self.diagnostics.len();
            let expanded = self.invoke_macro(
                &definition,
                selected.arguments,
                expression.syntax().span.clone(),
            );
            let Some(result) = expanded else {
                self.expansion_stack.pop();
                if self.diagnostics.len() > diagnostic_start {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!("while expanding macro `{}`", key.name),
                    ));
                }
                return expression;
            };
            let category = syntax_category(&result);
            let Some(mut result) = result.into_expression() else {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    if category == "item" {
                        format!(
                            "macro `{}` produces item syntax and may only be invoked as a standalone top-level item",
                            key.name
                        )
                    } else {
                        format!(
                            "macro `{}` produces {category} syntax, which cannot be used as an expression",
                            key.name
                        )
                    },
                ));
                self.expansion_stack.pop();
                return expression;
            };
            for argument in &arguments[consumed_count..] {
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

        if let Expression::Name(name) = head
            && matches!(
                name.name.as_str(),
                "Ident"
                    | "CallExpr"
                    | "Sequence"
                    | "Separated"
                    | "Comma"
                    | "Equals"
                    | "FatArrow"
                    | "Parenthesized"
                    | "Bracketed"
                    | "Braced"
                    | "Private"
                    | "Public"
                    | "PublicRepr"
            )
        {
            self.diagnostics.push(Diagnostic::new(
                expression.syntax().span.clone(),
                format!("`{}` syntax values are compile-time-only", name.name),
            ));
            return expression;
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
            Expression::VisibilityArgument(visibility) => {
                self.diagnostics.push(Diagnostic::new(
                    visibility.syntax.span.clone(),
                    "visibility syntax may only be passed to a matching macro parameter",
                ));
                Expression::VisibilityArgument(visibility)
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
                let (namespace, item, definition_module) = qualified_macro_access_path(access)?;
                let context = definition_module.map(ModuleId).unwrap_or(module);
                let target = self.scopes[context.0].namespaces.get(&namespace)?;
                self.scopes[target.0].macros.get(&item).map(|keys| {
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
        call_visibility: Option<&VisibilitySyntax>,
        span: Span,
    ) -> Option<SelectedMacro> {
        let definitions = keys
            .iter()
            .filter_map(|key| self.definitions.get(key).cloned())
            .collect::<Vec<_>>();
        let mut complete = definitions
            .iter()
            .filter_map(|definition| {
                match_macro_arguments(definition, arguments, call_visibility).map(
                    |(matched_arguments, consumed)| SelectedMacro {
                        definition: definition.clone(),
                        arguments: matched_arguments,
                        consumed,
                    },
                )
            })
            .collect::<Vec<_>>();
        if complete.is_empty() {
            let has_visibility_parameters = definitions.iter().any(|definition| {
                definition.parameters.iter().any(|parameter| {
                    matches!(
                        parameter,
                        MetaType::Visibility | MetaType::MacroCallVisibility
                    )
                })
            });
            if call_visibility.is_some() {
                let name = keys
                    .first()
                    .map(|key| key.name.as_str())
                    .unwrap_or("<macro>");
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "macro `{name}` has no overload whose first parameter is `MacroCallVisibility`"
                    ),
                ));
                return None;
            }
            if !has_visibility_parameters
                && let [definition] = definitions.as_slice()
                && arguments.len() >= definition.arity
            {
                self.validate_macro_arguments(definition, &arguments[..definition.arity]);
                return None;
            }
            let mut arities = definitions
                .iter()
                .filter(|definition| {
                    !has_visibility_parameters
                        && arguments.len() < definition.arity
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
        let longest = complete.iter().map(|selected| selected.consumed).max()?;
        complete.retain(|selected| selected.consumed == longest);
        let undominated = complete
            .iter()
            .filter(|candidate| {
                !complete.iter().any(|other| {
                    other.definition.key != candidate.definition.key
                        && signature_more_specific(
                            &other.definition.parameters,
                            &candidate.definition.parameters,
                        )
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if let [selected] = undominated.as_slice() {
            return Some(selected.clone());
        }
        let name = keys
            .first()
            .map(|key| key.name.as_str())
            .unwrap_or("<macro>");
        self.diagnostics.push(Diagnostic::new(
            span,
            format!("ambiguous invocation of macro `{name}`"),
        ));
        for selected in undominated {
            let definition = selected.definition;
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
        arguments: Vec<MacroArgument>,
        call_span: Span,
    ) -> Option<SyntaxValue> {
        match &definition.kind {
            MacroKind::CString => {
                let [argument] = arguments.as_slice() else {
                    return None;
                };
                let MacroArgument::Expression(argument) = argument else {
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
                Some(SyntaxValue::from_expression(Expression::CString(
                    crate::CStringExpression {
                        syntax,
                        literal: string.literal.clone(),
                    },
                )))
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
                for (expected, argument) in definition.parameters.iter().zip(arguments) {
                    let argument = match argument {
                        MacroArgument::Expression(argument) => {
                            self.meta_argument_value(expected, &argument)?
                        }
                        MacroArgument::Visibility(visibility) => {
                            SyntaxValue::Visibility(visibility)
                        }
                    };
                    value = self.apply_value(value, Value::Syntax(argument), call_span.clone())?;
                }
                match value {
                    Value::Syntax(syntax) => Some(syntax),
                    Value::Sequence(values)
                        if matches!(definition.result, MetaType::Sequence(_)) =>
                    {
                        values
                            .into_iter()
                            .map(|value| match value {
                                Value::Syntax(SyntaxValue::Item(item)) => Some(*item),
                                _ => None,
                            })
                            .collect::<Option<Vec<_>>>()
                            .map(SyntaxValue::Items)
                    }
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

    fn expand_generated_items(
        &mut self,
        module: ModuleId,
        generated: Vec<Item>,
        depth: usize,
    ) -> Vec<Item> {
        let mut expanded = Vec::new();
        for mut item in generated {
            self.expand_item(module, &mut item, depth);
            if let Some(nested) = self.emitted_items.take() {
                expanded.extend(self.expand_generated_items(module, nested, depth + 1));
            } else {
                expanded.push(item);
            }
        }
        expanded
    }

    fn meta_argument_value(
        &mut self,
        expected: &MetaType,
        argument: &Expression,
    ) -> Option<SyntaxValue> {
        if let Expression::VisibilityArgument(visibility) = argument {
            return matches!(
                expected,
                MetaType::Syntax | MetaType::Visibility | MetaType::MacroCallVisibility
            )
            .then(|| SyntaxValue::Visibility(visibility.clone()));
        }
        match expected {
            MetaType::Type => {
                let (syntax, grouped) = category_argument_syntax(argument)?;
                crate::parser::parse_type_fragment(syntax, grouped, &mut self.next_syntax_id)
                    .ok()
                    .map(SyntaxValue::Type)
            }
            MetaType::Pattern => {
                let (syntax, grouped) = category_argument_syntax(argument)?;
                crate::parser::parse_pattern_fragment(syntax, grouped, &mut self.next_syntax_id)
                    .ok()
                    .map(SyntaxValue::Pattern)
            }
            MetaType::Visibility | MetaType::MacroCallVisibility => {
                let Expression::VisibilityArgument(visibility) = argument else {
                    return None;
                };
                Some(SyntaxValue::Visibility(visibility.clone()))
            }
            MetaType::Item => None,
            MetaType::Comma => Some(SyntaxValue::Comma(argument.syntax().clone())),
            MetaType::Equals => Some(SyntaxValue::Equals(argument.syntax().clone())),
            MetaType::FatArrow => Some(SyntaxValue::FatArrow(argument.syntax().clone())),
            MetaType::Delimited(_, _) => {
                delimiter_argument_value(expected, argument.syntax(), &mut self.next_syntax_id)
            }
            _ => Some(SyntaxValue::from_expression(
                meta_argument_expression(expected, argument).clone(),
            )),
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
                    MetaType::CallExpr => "a call expression".to_owned(),
                    MetaType::UnstructuredExpr => "an unstructured expression".to_owned(),
                    MetaType::Type => "a type".to_owned(),
                    MetaType::Pattern => "a pattern".to_owned(),
                    MetaType::Item => "an item".to_owned(),
                    MetaType::Visibility => "visibility syntax".to_owned(),
                    MetaType::MacroCallVisibility => "macro-call visibility".to_owned(),
                    MetaType::Comma => "comma syntax".to_owned(),
                    MetaType::Equals => "equals syntax".to_owned(),
                    MetaType::FatArrow => "fat-arrow syntax".to_owned(),
                    MetaType::Product(_) | MetaType::Optional(_) | MetaType::Sequence(_) => {
                        format!("`{}` syntax", format_meta_type(expected))
                    }
                    MetaType::Syntax | MetaType::Expr => "an expression".to_owned(),
                    MetaType::Delimited(_, _) => format!("`{}` syntax", format_meta_type(expected)),
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
            Expression::Resource(_) | Expression::With(_) => {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    "resources are not available during compile-time macro evaluation",
                ));
                None
            }
            Expression::Function(function) => Some(Value::Function {
                module,
                function: function.as_ref().clone(),
                environment: environment.clone(),
            }),
            Expression::Satisfies(satisfies) => {
                if matches!(meta_type(&satisfies.ty), Some(MetaType::Type))
                    && let Expression::Quote(quote) = satisfies.value.as_ref()
                {
                    return self.instantiate_type_quote(module, quote, environment);
                }
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
                    return Some(value.get());
                }
                if let Some(helper) = self.scopes[module.0].helpers.get(&name.name).cloned() {
                    if matches!(helper.binding.value, Some(Expression::Function(_))) {
                        return Some(Value::Helper(helper.module, helper.binding));
                    }
                    let value = helper.binding.value?;
                    return self.eval_expression(helper.module, &value, &mut Environment::new());
                }
                if name.name == "Comma" {
                    return Some(Value::Syntax(SyntaxValue::Comma(name.syntax.clone())));
                }
                if name.name == "Equals" {
                    return Some(Value::Syntax(SyntaxValue::Equals(name.syntax.clone())));
                }
                if name.name == "FatArrow" {
                    return Some(Value::Syntax(SyntaxValue::FatArrow(name.syntax.clone())));
                }
                if let Some(kind) = match name.name.as_str() {
                    "Private" => Some(VisibilityKind::Private),
                    "Public" => Some(VisibilityKind::Public),
                    "PublicRepr" => Some(VisibilityKind::PublicRepr),
                    _ => None,
                } {
                    return Some(Value::Syntax(SyntaxValue::Visibility(VisibilitySyntax {
                        syntax: name.syntax.clone(),
                        kind,
                    })));
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
            Expression::SyntaxArgument(argument) => {
                self.diagnostics.push(Diagnostic::new(
                    argument.syntax.span.clone(),
                    "grouped type or pattern syntax may only be passed to a matching macro parameter",
                ));
                None
            }
            Expression::VisibilityArgument(visibility) => {
                Some(Value::Syntax(SyntaxValue::Visibility(visibility.clone())))
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
                    let selected = self.select_macro(
                        &keys,
                        &arguments,
                        None,
                        expression.syntax().span.clone(),
                    )?;
                    self.record_invocation(head.syntax().id, &selected.definition);
                    let definition = selected.definition;
                    let consumed_count = selected.consumed;
                    let key = definition.key.clone();
                    if self.expansion_stack.contains(&key) {
                        self.diagnostics.push(Diagnostic::new(
                            expression.syntax().span.clone(),
                            format!("recursive macro expansion of `{}`", key.name),
                        ));
                        return None;
                    }
                    self.expansion_stack.push(key.clone());
                    let mut result = self.invoke_macro(
                        &definition,
                        selected.arguments,
                        expression.syntax().span.clone(),
                    );
                    if !arguments[consumed_count..].is_empty()
                        && result
                            .as_ref()
                            .is_some_and(|syntax| syntax.to_expression().is_none())
                    {
                        self.diagnostics.push(Diagnostic::new(
                            expression.syntax().span.clone(),
                            format!(
                                "{}-producing macro `{}` cannot have excess arguments",
                                result.as_ref().map(syntax_category).unwrap_or("syntax"),
                                key.name
                            ),
                        ));
                        result = None;
                    } else if let Some(syntax) = result.take() {
                        if syntax.to_expression().is_none() {
                            result = Some(syntax);
                        } else {
                            let mut expanded = syntax
                                .into_expression()
                                .expect("non-item syntax must contain an expression");
                            for argument in &arguments[consumed_count..] {
                                let mut syntax = expanded.syntax().clone();
                                syntax.id = self.fresh_id();
                                expanded = Expression::Call(crate::CallExpression {
                                    syntax,
                                    callee: Box::new(expanded),
                                    argument: Box::new((*argument).clone()),
                                });
                            }
                            result = Some(SyntaxValue::from_expression(expanded));
                        };
                    }
                    let result = result.map(Value::Syntax);
                    self.expansion_stack.pop();
                    return result;
                }
                if let Expression::Name(name) = call.callee.as_ref()
                    && name.name.chars().next().is_some_and(char::is_uppercase)
                {
                    let argument = self.eval_expression(module, &call.argument, environment)?;
                    if matches!(
                        name.name.as_str(),
                        "Ident"
                            | "CallExpr"
                            | "Sequence"
                            | "Separated"
                            | "Parenthesized"
                            | "Bracketed"
                            | "Braced"
                    ) {
                        return self.construct_syntax(
                            module,
                            &name.name,
                            argument,
                            call.syntax.span.clone(),
                        );
                    }
                    return Some(Value::Nominal(name.name.clone(), Box::new(argument)));
                }
                let callee = self.eval_expression(module, &call.callee, environment)?;
                let argument = self.eval_expression(module, &call.argument, environment)?;
                self.apply_value(callee, argument, call.syntax.span.clone())
            }
            Expression::Access(access) => {
                if let Some((namespace, item, definition_module)) =
                    qualified_macro_access_path(access)
                    && let context = definition_module.map(ModuleId).unwrap_or(module)
                    && let Some(target) = self.scopes[context.0].namespaces.get(&namespace).copied()
                    && let Some(helper) = self.scopes[target.0].helpers.get(&item).cloned()
                    && helper.binding.visibility == Visibility::Public
                {
                    return Some(Value::Helper(helper.module, helper.binding));
                }
                let value = self.eval_expression(module, &access.value, environment)?;
                if let Value::Syntax(SyntaxValue::Call(call)) = value {
                    return match &access.accessor {
                        Accessor::Name(name) if name == "callee" => Some(Value::Syntax(
                            SyntaxValue::from_expression((*call.callee).clone()),
                        )),
                        Accessor::Name(name) if name == "argument" => Some(Value::Syntax(
                            SyntaxValue::from_expression((*call.argument).clone()),
                        )),
                        Accessor::Name(name) => {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                format!("call syntax has no field `{name}`"),
                            ));
                            None
                        }
                        Accessor::Index(_) => {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                "call syntax fields must be accessed by name",
                            ));
                            None
                        }
                    };
                }
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
                            let value = if binding
                                .annotation
                                .as_ref()
                                .is_some_and(|ty| matches!(meta_type(ty), Some(MetaType::Type)))
                                && let Expression::Quote(quote) = value
                            {
                                self.instantiate_type_quote(module, quote, &local)?
                            } else {
                                self.eval_expression(module, value, &mut local)?
                            };
                            local.insert(
                                binding.name.clone(),
                                EnvironmentBinding::new(value, binding.mutable),
                            );
                        }
                        Statement::PatternBinding(binding) => {
                            let value = self.eval_expression(module, &binding.value, &mut local)?;
                            if !match_pattern(&binding.pattern, &value, &mut local) {
                                return None;
                            }
                        }
                        Statement::Assignment(assignment) => {
                            let value =
                                self.eval_expression(module, &assignment.value, &mut local)?;
                            if !self.assign_compile_time(
                                module,
                                &assignment.target,
                                value,
                                &mut local,
                                assignment.syntax.span.clone(),
                            ) {
                                return None;
                            }
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

    fn construct_syntax(
        &mut self,
        module: ModuleId,
        constructor: &str,
        argument: Value,
        span: Span,
    ) -> Option<Value> {
        match constructor {
            "Ident" => {
                let Value::String(literal) = argument else {
                    self.diagnostics
                        .push(Diagnostic::new(span, "`Ident` requires a string spelling"));
                    return None;
                };
                let spelling = crate::string_literal::decode(&literal).unwrap_or(literal);
                let tokens = crate::lexer::lex(&spelling)
                    .into_iter()
                    .filter(|token| !token.kind.is_trivia())
                    .collect::<Vec<_>>();
                if !matches!(tokens.as_slice(), [token]
                    if token.kind == crate::TokenKind::Identifier && token.text == spelling)
                {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        format!("`{spelling}` is not a valid identifier spelling"),
                    ));
                    return None;
                }
                let syntax = self.generated_syntax(module, span);
                Some(Value::Syntax(SyntaxValue::Ident(crate::NameExpression {
                    syntax,
                    name: spelling,
                })))
            }
            "CallExpr" => {
                let Value::Product(elements) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`CallExpr` requires `(callee: Expr, argument: Expr)`",
                    ));
                    return None;
                };
                let [(callee_name, callee), (argument_name, argument)] = elements.as_slice() else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`CallExpr` requires exactly `callee` and `argument` fields",
                    ));
                    return None;
                };
                if callee_name.as_deref() != Some("callee")
                    || argument_name.as_deref() != Some("argument")
                {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`CallExpr` fields must be named `callee` and `argument`",
                    ));
                    return None;
                }
                let Value::Syntax(callee) = callee else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`CallExpr.callee` must contain `Expr`",
                    ));
                    return None;
                };
                let Value::Syntax(argument) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`CallExpr.argument` must contain `Expr`",
                    ));
                    return None;
                };
                let syntax = self.generated_syntax(module, span);
                Some(Value::Syntax(SyntaxValue::Call(crate::CallExpression {
                    syntax,
                    callee: Box::new(callee.to_expression()?),
                    argument: Box::new(argument.to_expression()?),
                })))
            }
            "Sequence" => {
                let Value::Product(elements) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`Sequence` requires a product of syntax values",
                    ));
                    return None;
                };
                if let [
                    (Some(first), first_value),
                    (Some(rest), Value::Sequence(rest_values)),
                ] = elements.as_slice()
                    && first == "first"
                    && rest == "rest"
                {
                    let mut values = Vec::with_capacity(rest_values.len() + 1);
                    values.push(first_value.clone());
                    values.extend(rest_values.clone());
                    return Some(Value::Sequence(values));
                }
                Some(Value::Sequence(
                    elements.into_iter().map(|(_, value)| value).collect(),
                ))
            }
            "Separated" => {
                let Value::Product(fields) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`Separated` requires `(separator: Syntax, elements: (...), trailing: Bool)`",
                    ));
                    return None;
                };
                let [
                    (separator_name, separator),
                    (elements_name, elements),
                    (trailing_name, trailing),
                ] = fields.as_slice()
                else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`Separated` requires exactly `separator`, `elements`, and `trailing` fields",
                    ));
                    return None;
                };
                if separator_name.as_deref() != Some("separator")
                    || elements_name.as_deref() != Some("elements")
                    || trailing_name.as_deref() != Some("trailing")
                {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`Separated` fields must be named `separator`, `elements`, and `trailing`",
                    ));
                    return None;
                }
                if !matches!(separator, Value::Syntax(SyntaxValue::Comma(_))) {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`Separated.separator` must contain `Comma`",
                    ));
                    return None;
                }
                let elements = match elements {
                    Value::Product(elements) => {
                        elements.iter().map(|(_, value)| value.clone()).collect()
                    }
                    Value::Sequence(elements) => elements.clone(),
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            "`Separated.elements` must be a product or `Sequence`",
                        ));
                        return None;
                    }
                };
                let trailing = match trailing {
                    Value::Nominal(name, value) if matches!(value.as_ref(), Value::Product(values) if values.is_empty()) => {
                        match name.as_str() {
                            "True" => true,
                            "False" => false,
                            _ => {
                                self.diagnostics.push(Diagnostic::new(
                                    span,
                                    "`Separated.trailing` must be `True` or `False`",
                                ));
                                return None;
                            }
                        }
                    }
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            "`Separated.trailing` must be `True` or `False`",
                        ));
                        return None;
                    }
                };
                if trailing && elements.is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "an empty `Separated` value cannot have a trailing separator",
                    ));
                    return None;
                }
                Some(Value::Separated {
                    elements,
                    separator: Box::new(separator.clone()),
                    trailing,
                })
            }
            "Parenthesized" | "Bracketed" | "Braced" => {
                let kind = delimiter_kind(constructor).expect("matched delimiter constructor");
                let contents = match argument {
                    Value::Product(mut values)
                        if values.len() == 1 && matches!(values[0].1, Value::Sequence(_)) =>
                    {
                        let (_, Value::Sequence(values)) = values.remove(0) else {
                            unreachable!("guarded sequence contents")
                        };
                        DelimitedValueContents::Sequence(values)
                    }
                    Value::Product(mut values)
                        if values.len() == 1 && matches!(values[0].1, Value::Separated { .. }) =>
                    {
                        let (
                            _,
                            Value::Separated {
                                elements,
                                separator,
                                trailing,
                            },
                        ) = values.remove(0)
                        else {
                            unreachable!("guarded separated contents")
                        };
                        DelimitedValueContents::Separated {
                            elements,
                            separator,
                            trailing,
                        }
                    }
                    Value::Product(values) => {
                        if !values
                            .iter()
                            .all(|(_, value)| matches!(value, Value::Syntax(_)))
                        {
                            self.diagnostics.push(Diagnostic::new(
                                span,
                                format!("`{constructor}` contents must contain `Syntax`"),
                            ));
                            return None;
                        }
                        DelimitedValueContents::Fixed(values)
                    }
                    Value::Sequence(values) => DelimitedValueContents::Sequence(values),
                    Value::Separated {
                        elements,
                        separator,
                        trailing,
                    } => DelimitedValueContents::Separated {
                        elements,
                        separator,
                        trailing,
                    },
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            format!("`{constructor}` requires product or sequence contents"),
                        ));
                        return None;
                    }
                };
                let generated_count = match &contents {
                    DelimitedValueContents::Fixed(values) => values.len(),
                    DelimitedValueContents::Sequence(values) => values.len(),
                    DelimitedValueContents::Separated { elements, .. } => elements.len(),
                }
                .saturating_mul(2)
                .saturating_add(1);
                Some(Value::Syntax(SyntaxValue::Delimited(
                    DelimitedSyntaxValue {
                        kind,
                        syntax: self.generated_syntax(module, span.clone()),
                        contents,
                        generated: (0..generated_count)
                            .map(|_| self.generated_syntax(module, span.clone()))
                            .collect(),
                        expression: None,
                    },
                )))
            }
            _ => unreachable!(),
        }
    }

    fn generated_syntax(&mut self, module: ModuleId, span: Span) -> Syntax {
        let mark = self.next_mark;
        self.next_mark += 1;
        Syntax::synthetic(self.fresh_id(), span).generated(module.0, mark)
    }

    fn assign_compile_time(
        &mut self,
        module: ModuleId,
        target: &Expression,
        replacement: Value,
        environment: &mut Environment,
        span: Span,
    ) -> bool {
        let mut fields = Vec::new();
        let mut root = target;
        while let Expression::Access(access) = root {
            let Accessor::Name(field) = &access.accessor else {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "compile-time assignment only supports named fields",
                ));
                return false;
            };
            fields.push(field.clone());
            root = &access.value;
        }
        fields.reverse();
        let Expression::Name(name) = root else {
            self.diagnostics.push(Diagnostic::new(
                span,
                "invalid compile-time assignment target",
            ));
            return false;
        };
        let Some(binding) = environment.get(&name.name).cloned() else {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("unknown compile-time binding `{}`", name.name),
            ));
            return false;
        };
        if !binding.mutable {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "cannot assign to immutable compile-time binding `{}`",
                    name.name
                ),
            ));
            return false;
        }
        let current = binding.get();
        let Some(updated) =
            self.replace_compile_time_path(module, current, &fields, replacement, span)
        else {
            return false;
        };
        *binding.value.borrow_mut() = updated;
        true
    }

    fn replace_compile_time_path(
        &mut self,
        module: ModuleId,
        current: Value,
        fields: &[String],
        replacement: Value,
        span: Span,
    ) -> Option<Value> {
        let Some((field, remaining)) = fields.split_first() else {
            return Some(replacement);
        };
        match current {
            Value::Syntax(SyntaxValue::Call(mut call)) => {
                let child = match field.as_str() {
                    "callee" => SyntaxValue::from_expression((*call.callee).clone()),
                    "argument" => SyntaxValue::from_expression((*call.argument).clone()),
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            format!("call syntax has no field `{field}`"),
                        ));
                        return None;
                    }
                };
                let updated = self.replace_compile_time_path(
                    module,
                    Value::Syntax(child),
                    remaining,
                    replacement,
                    span.clone(),
                )?;
                let Value::Syntax(updated) = updated else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        format!("`CallExpr.{field}` must contain `Expr`"),
                    ));
                    return None;
                };
                match field.as_str() {
                    "callee" => call.callee = Box::new(updated.into_expression()?),
                    "argument" => call.argument = Box::new(updated.into_expression()?),
                    _ => unreachable!(),
                }
                call.syntax = self.generated_syntax(module, span);
                Some(Value::Syntax(SyntaxValue::Call(call)))
            }
            Value::Product(mut elements) => {
                let Some((_, child)) = elements
                    .iter_mut()
                    .find(|(name, _)| name.as_deref() == Some(field))
                else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        format!("compile-time product has no field `{field}`"),
                    ));
                    return None;
                };
                *child = self.replace_compile_time_path(
                    module,
                    child.clone(),
                    remaining,
                    replacement,
                    span,
                )?;
                Some(Value::Product(elements))
            }
            _ => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("compile-time value has no field `{field}`"),
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
        template: &crate::QuoteTemplate,
        environment: &Environment,
        mark: u64,
    ) -> Option<SyntaxValue> {
        match template {
            crate::QuoteTemplate::Expression(template) => {
                let mut expression = template.as_ref().clone();
                alpha_rename_expression(&mut expression, mark, &mut Vec::new());
                self.freshen_expression(&mut expression, module, mark);
                substitute_splices(&expression, environment, &mut self.diagnostics)
                    .map(SyntaxValue::from_expression)
            }
            crate::QuoteTemplate::Item(template) => {
                let mut item = template.as_ref().clone();
                if !item_output_supported(&item) {
                    self.diagnostics.push(Diagnostic::new(
                        item_syntax(&item).span.clone(),
                        "item quotations cannot generate `macro` declarations yet",
                    ));
                    return None;
                }
                alpha_rename_item(&mut item, mark);
                freshen_item(self, &mut item, module, mark);
                substitute_item(&mut item, environment, &mut self.diagnostics)?;
                Some(SyntaxValue::Item(Box::new(item)))
            }
            crate::QuoteTemplate::Items(templates) => {
                let mut items = templates.clone();
                for item in &items {
                    if !item_output_supported(item) {
                        self.diagnostics.push(Diagnostic::new(
                            item_syntax(item).span.clone(),
                            "item quotations cannot generate `macro` declarations yet",
                        ));
                        return None;
                    }
                }
                for item in &mut items {
                    alpha_rename_item(item, mark);
                    freshen_item(self, item, module, mark);
                }
                substitute_item_list(&mut items, environment, &mut self.diagnostics)?;
                Some(SyntaxValue::Items(items))
            }
            crate::QuoteTemplate::Raw => {
                self.diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    "quotation requires a contextual syntax type",
                ));
                None
            }
        }
    }

    fn instantiate_type_quote(
        &mut self,
        module: ModuleId,
        quote: &crate::QuoteExpression,
        environment: &Environment,
    ) -> Option<Value> {
        let mark = self.next_mark;
        self.next_mark += 1;
        let mut ty =
            crate::parser::parse_type_template_fragment(&quote.contents, &mut self.next_syntax_id)
                .map_err(|error| {
                    self.diagnostics
                        .push(Diagnostic::new(quote.contents.span.clone(), error.message));
                })
                .ok()?;
        freshen_type(self, &mut ty, module, mark);
        substitute_type(&mut ty, environment, &mut self.diagnostics)?;
        Some(Value::Syntax(SyntaxValue::Type(ty)))
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
            Expression::Resource(resource) => {
                freshen_type(self, &mut resource.resource, module, mark);
            }
            Expression::With(with) => {
                freshen_type(self, &mut with.resource, module, mark);
                self.freshen_expression(&mut with.value, module, mark);
                self.freshen_syntax(&mut with.body.syntax, module, mark);
                for statement in &mut with.body.statements {
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
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_) => {}
            Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }
}

fn invalid_delimited_collection_parameter(expression: &Expression) -> bool {
    let mut current = expression;
    while let Expression::Function(function) = current {
        let ty = match &function.pattern {
            Pattern::Binding(binding) => Some(&binding.ty),
            Pattern::Wildcard(wildcard) => Some(&wildcard.ty),
            _ => None,
        };
        if ty.is_some_and(|ty| {
            (type_contains_named(ty, "Sequence") || type_contains_named(ty, "Separated"))
                && (meta_type(ty).is_none() || matches!(meta_type(ty), Some(MetaType::Sequence(_))))
        }) {
            return true;
        }
        current = &function.body;
    }
    false
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
            "CallExpr" => Some(MetaType::CallExpr),
            "UnstructuredExpr" => Some(MetaType::UnstructuredExpr),
            "Type" => Some(MetaType::Type),
            "Pattern" => Some(MetaType::Pattern),
            "Item" => Some(MetaType::Item),
            "Visibility" => Some(MetaType::Visibility),
            "MacroCallVisibility" => Some(MetaType::MacroCallVisibility),
            "Comma" => Some(MetaType::Comma),
            "Equals" => Some(MetaType::Equals),
            "FatArrow" => Some(MetaType::FatArrow),
            _ => None,
        },
        Type::Application(application) => {
            if let Some(element) = sequence_meta_type(ty) {
                return Some(MetaType::Sequence(Box::new(element)));
            }
            if let Some(element) = applied_meta_type(application, "Optional") {
                return Some(MetaType::Optional(Box::new(element)));
            }
            if let Some(element) = applied_meta_type(application, "Sequence") {
                return Some(MetaType::Sequence(Box::new(element)));
            }
            let Type::Named(callee) = application.callee.as_ref() else {
                return None;
            };
            if callee.namespace.is_some() {
                return None;
            }
            if let Some(kind) = delimiter_kind(&callee.name) {
                return delimiter_contents_meta(application.argument.as_ref())
                    .map(|contents| MetaType::Delimited(kind, contents));
            }
            if callee.name != "Ident" {
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
        Type::Product(product)
            if product
                .elements
                .iter()
                .all(|element| element.name.is_none() && !element.spread) =>
        {
            product
                .elements
                .iter()
                .map(|element| meta_type(&element.ty))
                .collect::<Option<Vec<_>>>()
                .map(MetaType::Product)
        }
        _ => None,
    }
}

fn applied_meta_type(application: &crate::TypeApplication, expected: &str) -> Option<MetaType> {
    let Type::Named(callee) = application.callee.as_ref() else {
        return None;
    };
    (callee.namespace.is_none() && callee.name == expected)
        .then(|| meta_type(unwrap_singleton_product(&application.argument)))
        .flatten()
}

fn delimiter_kind(name: &str) -> Option<DelimiterKind> {
    match name {
        "Parenthesized" => Some(DelimiterKind::Parenthesized),
        "Bracketed" => Some(DelimiterKind::Bracketed),
        "Braced" => Some(DelimiterKind::Braced),
        _ => None,
    }
}

fn delimiter_contents_meta(ty: &Type) -> Option<DelimitedMetaContents> {
    let Type::Product(product) = ty else {
        return None;
    };
    if product.elements.len() == 1
        && !product.elements[0].spread
        && let Some(element) = sequence_meta_type(&product.elements[0].ty)
    {
        return Some(DelimitedMetaContents::Sequence(Box::new(element)));
    }
    if product.elements.len() == 1
        && !product.elements[0].spread
        && let Some((element, separator)) = separated_meta_type(&product.elements[0].ty)
    {
        return Some(DelimitedMetaContents::Separated {
            element: Box::new(element),
            separator: Box::new(separator),
        });
    }
    product
        .elements
        .iter()
        .map(|element| {
            (!element.spread && element.name.is_none())
                .then(|| meta_type(&element.ty))
                .flatten()
        })
        .collect::<Option<Vec<_>>>()
        .map(DelimitedMetaContents::Fixed)
}

fn separated_meta_type(ty: &Type) -> Option<(MetaType, MetaType)> {
    let mut arguments = Vec::new();
    let mut current = ty;
    while let Type::Application(application) = current {
        arguments.push(application.argument.as_ref());
        current = application.callee.as_ref();
    }
    arguments.reverse();
    let Type::Named(name) = current else {
        return None;
    };
    if name.namespace.is_some() || name.name != "Separated" {
        return None;
    }
    let [element, separator] = arguments.as_slice() else {
        return None;
    };
    let separator = meta_type(separator)?;
    if separator != MetaType::Comma {
        return None;
    }
    Some((meta_type(unwrap_singleton_product(element))?, separator))
}

fn unwrap_singleton_product(mut ty: &Type) -> &Type {
    while let Type::Product(product) = ty
        && let [element] = product.elements.as_slice()
        && element.name.is_none()
        && !element.spread
    {
        ty = &element.ty;
    }
    ty
}

/// `Sequence Ident String` is intentionally read as `Sequence (Ident String)`
/// in syntax metadata, despite ordinary type application being left-associative.
fn sequence_meta_type(ty: &Type) -> Option<MetaType> {
    let mut arguments = Vec::new();
    let mut current = ty;
    while let Type::Application(application) = current {
        arguments.push(application.argument.as_ref());
        current = application.callee.as_ref();
    }
    arguments.reverse();
    let Type::Named(name) = current else {
        return None;
    };
    if name.namespace.is_some() || name.name != "Sequence" || arguments.is_empty() {
        return None;
    }
    match arguments.as_slice() {
        [element] => meta_type(element),
        [first, second] if matches!(first, Type::Named(name) if name.namespace.is_none() && name.name == "Ident") => {
            match second {
                Type::Named(name) if name.namespace.is_none() && name.name == "String" => {
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
    let expression_argument = meta_argument_expression(expected, argument);
    match expected {
        MetaType::Type => category_argument_syntax(argument).is_some_and(|(syntax, grouped)| {
            let mut next_syntax_id = 0;
            crate::parser::parse_type_fragment(syntax, grouped, &mut next_syntax_id).is_ok()
        }),
        MetaType::Pattern => category_argument_syntax(argument).is_some_and(|(syntax, grouped)| {
            let mut next_syntax_id = 0;
            crate::parser::parse_pattern_fragment(syntax, grouped, &mut next_syntax_id).is_ok()
        }),
        MetaType::Visibility => matches!(argument, Expression::VisibilityArgument(_)),
        MetaType::MacroCallVisibility => false,
        MetaType::Syntax => !matches!(argument, Expression::SyntaxArgument(_)),
        MetaType::Expr => !matches!(
            argument,
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_)
        ),
        MetaType::Ident(spelling) => match expression_argument {
            Expression::Name(name) if is_plain_identifier(name) => spelling
                .as_ref()
                .is_none_or(|expected| expected == &name.name),
            _ => false,
        },
        MetaType::CallExpr => matches!(expression_argument, Expression::Call(_)),
        MetaType::UnstructuredExpr => !matches!(
            expression_argument,
            Expression::Name(_) | Expression::Call(_) | Expression::VisibilityArgument(_)
        ),
        MetaType::Item | MetaType::Product(_) | MetaType::Optional(_) | MetaType::Sequence(_) => {
            false
        }
        MetaType::Comma => matches_single_token(argument.syntax(), crate::TokenKind::Comma),
        MetaType::Equals => matches_single_token(argument.syntax(), crate::TokenKind::Equals),
        MetaType::FatArrow => matches_single_token(argument.syntax(), crate::TokenKind::FatArrow),
        MetaType::Delimited(_, _) => {
            let mut next_syntax_id = 0;
            delimiter_argument_value(expected, argument.syntax(), &mut next_syntax_id).is_some()
        }
    }
}

fn delimiter_argument_value(
    expected: &MetaType,
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Option<SyntaxValue> {
    let MetaType::Delimited(expected_kind, expected_contents) = expected else {
        return None;
    };
    let tokens = syntax.tokens();
    let first = tokens.iter().position(|token| !token.kind.is_trivia())?;
    let last = tokens.iter().rposition(|token| !token.kind.is_trivia())?;
    let kind = match (tokens[first].kind, tokens[last].kind) {
        (crate::TokenKind::LParen, crate::TokenKind::RParen) => DelimiterKind::Parenthesized,
        (crate::TokenKind::LBracket, crate::TokenKind::RBracket) => DelimiterKind::Bracketed,
        (crate::TokenKind::LBrace, crate::TokenKind::RBrace) => DelimiterKind::Braced,
        _ => return None,
    };
    if kind != *expected_kind {
        return None;
    }
    let contents = match expected_contents {
        DelimitedMetaContents::Fixed(elements) => {
            match_fixed_contents(syntax, first + 1, last, elements, next_syntax_id)
                .map(DelimitedValueContents::Fixed)
        }
        DelimitedMetaContents::Sequence(element) => {
            match_sequence_contents(syntax, first + 1, last, element, next_syntax_id)
                .map(DelimitedValueContents::Sequence)
        }
        DelimitedMetaContents::Separated { element, separator } => {
            match_separated_contents(syntax, first + 1, last, element, separator, next_syntax_id)
                .map(
                    |(elements, separator, trailing)| DelimitedValueContents::Separated {
                        elements,
                        separator: Box::new(separator),
                        trailing,
                    },
                )
        }
    }?;
    let expression = crate::parser::parse_expression_fragment(syntax, next_syntax_id)
        .ok()
        .map(Box::new);
    Some(SyntaxValue::Delimited(DelimitedSyntaxValue {
        kind,
        syntax: syntax.clone(),
        contents,
        generated: Vec::new(),
        expression,
    }))
}

fn match_separated_contents(
    parent: &Syntax,
    mut cursor: usize,
    end: usize,
    element: &MetaType,
    separator: &MetaType,
    next_syntax_id: &mut usize,
) -> Option<(Vec<Value>, Value, bool)> {
    cursor = skip_trivia(parent.tokens(), cursor, end);
    let separator_value = constructed_separator(separator)?;
    if cursor == end {
        return Some((Vec::new(), separator_value, false));
    }
    let mut elements = Vec::new();
    loop {
        let separator_at = find_top_level_separator(parent.tokens(), cursor, end, separator);
        let element_end = separator_at.unwrap_or(end);
        let element_end_trimmed = trim_trailing_trivia(parent.tokens(), cursor, element_end);
        if element_end_trimmed == cursor {
            return None;
        }
        let fragment = syntax_slice(parent, cursor, element_end_trimmed);
        elements.push(match_syntax_fragment(element, &fragment, next_syntax_id)?);
        let Some(separator_at) = separator_at else {
            return Some((elements, separator_value, false));
        };
        let separator_end = separator_at + 1;
        let separator_fragment = syntax_slice(parent, separator_at, separator_end);
        match_syntax_fragment(separator, &separator_fragment, next_syntax_id)?;
        cursor = skip_trivia(parent.tokens(), separator_end, end);
        if cursor == end {
            return Some((elements, separator_value, true));
        }
    }
}

fn constructed_separator(separator: &MetaType) -> Option<Value> {
    match separator {
        MetaType::Comma => Some(Value::Syntax(SyntaxValue::Comma(Syntax::compiler()))),
        _ => None,
    }
}

fn find_top_level_separator(
    tokens: &[crate::SyntaxToken],
    cursor: usize,
    end: usize,
    separator: &MetaType,
) -> Option<usize> {
    let separator_kind = match separator {
        MetaType::Comma => crate::TokenKind::Comma,
        _ => return None,
    };
    let mut delimiters = Vec::new();
    for (index, token) in tokens.iter().enumerate().take(end).skip(cursor) {
        match token.kind {
            crate::TokenKind::LParen => delimiters.push(crate::TokenKind::RParen),
            crate::TokenKind::LBracket => delimiters.push(crate::TokenKind::RBracket),
            crate::TokenKind::LBrace => delimiters.push(crate::TokenKind::RBrace),
            crate::TokenKind::RParen | crate::TokenKind::RBracket | crate::TokenKind::RBrace => {
                if delimiters.last() == Some(&token.kind) {
                    delimiters.pop();
                }
            }
            kind if delimiters.is_empty() && kind == separator_kind => return Some(index),
            _ => {}
        }
    }
    None
}

fn trim_trailing_trivia(tokens: &[crate::SyntaxToken], start: usize, mut end: usize) -> usize {
    while end > start && tokens[end - 1].kind.is_trivia() {
        end -= 1;
    }
    end
}

fn match_fixed_contents(
    parent: &Syntax,
    cursor: usize,
    end: usize,
    expected: &[MetaType],
    next_syntax_id: &mut usize,
) -> Option<Vec<(Option<String>, Value)>> {
    let cursor = skip_trivia(parent.tokens(), cursor, end);
    let Some((first, rest)) = expected.split_first() else {
        return (skip_trivia(parent.tokens(), cursor, end) == end).then(Vec::new);
    };
    if matches!(first, MetaType::Optional(_)) {
        let checkpoint = *next_syntax_id;
        if let Some(mut values) = match_fixed_contents(parent, cursor, end, rest, next_syntax_id) {
            values.insert(
                0,
                (
                    None,
                    Value::Nominal("None".to_owned(), Box::new(Value::Product(Vec::new()))),
                ),
            );
            return Some(values);
        }
        *next_syntax_id = checkpoint;
    }
    for candidate_end in candidate_ends(parent.tokens(), cursor, end) {
        let fragment = syntax_slice(parent, cursor, candidate_end);
        let checkpoint = *next_syntax_id;
        if let Some(value) = match_syntax_fragment(first, &fragment, next_syntax_id)
            && let Some(mut values) =
                match_fixed_contents(parent, candidate_end, end, rest, next_syntax_id)
        {
            values.insert(0, (None, value));
            return Some(values);
        }
        *next_syntax_id = checkpoint;
    }
    None
}

fn match_sequence_contents(
    parent: &Syntax,
    mut cursor: usize,
    end: usize,
    expected: &MetaType,
    next_syntax_id: &mut usize,
) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    loop {
        cursor = skip_trivia(parent.tokens(), cursor, end);
        if cursor == end {
            return Some(values);
        }
        let mut matched = None;
        for candidate_end in candidate_ends(parent.tokens(), cursor, end) {
            let fragment = syntax_slice(parent, cursor, candidate_end);
            let checkpoint = *next_syntax_id;
            if let Some(value) = match_syntax_fragment(expected, &fragment, next_syntax_id) {
                matched = Some((value, candidate_end));
                break;
            }
            *next_syntax_id = checkpoint;
        }
        let (value, candidate_end) = matched?;
        values.push(value);
        cursor = candidate_end;
    }
}

fn match_syntax_fragment(
    expected: &MetaType,
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Option<Value> {
    match expected {
        MetaType::Product(elements) => {
            let values =
                match_fixed_contents(syntax, 0, syntax.tokens().len(), elements, next_syntax_id)?;
            return Some(Value::Product(values));
        }
        MetaType::Optional(element) => {
            let empty = syntax.tokens().iter().all(|token| token.kind.is_trivia());
            return if empty {
                Some(Value::Nominal(
                    "None".to_owned(),
                    Box::new(Value::Product(Vec::new())),
                ))
            } else {
                match_syntax_fragment(element, syntax, next_syntax_id)
                    .map(|value| Value::Nominal("Some".to_owned(), Box::new(value)))
            };
        }
        MetaType::Sequence(_) => return None,
        _ => {}
    }
    let expression = || {
        let mut ids = *next_syntax_id;
        let value = crate::parser::parse_expression_fragment(syntax, &mut ids).ok()?;
        Some((value, ids))
    };
    let syntax_value = match expected {
        MetaType::Delimited(_, _) => {
            return delimiter_argument_value(expected, syntax, next_syntax_id).map(Value::Syntax);
        }
        MetaType::Ident(spelling) => {
            let mut tokens = syntax
                .tokens()
                .iter()
                .filter(|token| !token.kind.is_trivia());
            let token = tokens.next()?;
            if token.kind != crate::TokenKind::Identifier
                || tokens.next().is_some()
                || spelling
                    .as_ref()
                    .is_some_and(|expected| expected != &token.text)
            {
                return None;
            }
            SyntaxValue::Ident(crate::NameExpression {
                syntax: syntax.clone(),
                name: token.text.clone(),
            })
        }
        MetaType::Comma => {
            if !matches_single_token(syntax, crate::TokenKind::Comma) {
                return None;
            }
            SyntaxValue::Comma(syntax.clone())
        }
        MetaType::Equals => {
            if !matches_single_token(syntax, crate::TokenKind::Equals) {
                return None;
            }
            SyntaxValue::Equals(syntax.clone())
        }
        MetaType::FatArrow => {
            if !matches_single_token(syntax, crate::TokenKind::FatArrow) {
                return None;
            }
            SyntaxValue::FatArrow(syntax.clone())
        }
        MetaType::Expr | MetaType::CallExpr | MetaType::UnstructuredExpr => {
            let (value, ids) = expression()?;
            let value = SyntaxValue::from_expression(value);
            if matches!(expected, MetaType::CallExpr) && !matches!(value, SyntaxValue::Call(_))
                || matches!(expected, MetaType::UnstructuredExpr)
                    && !matches!(value, SyntaxValue::Unstructured(_))
            {
                return None;
            }
            *next_syntax_id = ids;
            value
        }
        MetaType::Type => SyntaxValue::Type(
            crate::parser::parse_type_fragment(syntax, false, next_syntax_id).ok()?,
        ),
        MetaType::Pattern => SyntaxValue::Pattern(
            crate::parser::parse_pattern_fragment(syntax, false, next_syntax_id).ok()?,
        ),
        MetaType::Item => SyntaxValue::Item(Box::new(
            crate::parser::parse_item_fragment(syntax, next_syntax_id).ok()?,
        )),
        MetaType::Visibility | MetaType::MacroCallVisibility => {
            let (value, ids) = expression()?;
            let Expression::VisibilityArgument(value) = value else {
                return None;
            };
            *next_syntax_id = ids;
            SyntaxValue::Visibility(value)
        }
        MetaType::Syntax => structural_syntax_value(syntax, next_syntax_id)?,
        MetaType::Product(_) | MetaType::Optional(_) | MetaType::Sequence(_) => unreachable!(),
    };
    Some(Value::Syntax(syntax_value))
}

fn matches_single_token(syntax: &Syntax, expected: crate::TokenKind) -> bool {
    let mut tokens = syntax
        .tokens()
        .iter()
        .filter(|token| !token.kind.is_trivia());
    matches!(tokens.next(), Some(token) if token.kind == expected) && tokens.next().is_none()
}

fn structural_syntax_value(syntax: &Syntax, next_syntax_id: &mut usize) -> Option<SyntaxValue> {
    let tokens = syntax
        .tokens()
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    if let [token] = tokens.as_slice()
        && token.kind == crate::TokenKind::Identifier
    {
        return Some(SyntaxValue::Ident(crate::NameExpression {
            syntax: syntax.clone(),
            name: token.text.clone(),
        }));
    }
    if let [token] = tokens.as_slice()
        && token.kind == crate::TokenKind::Comma
    {
        return Some(SyntaxValue::Comma(syntax.clone()));
    }
    if let [token] = tokens.as_slice()
        && token.kind == crate::TokenKind::Equals
    {
        return Some(SyntaxValue::Equals(syntax.clone()));
    }
    if let [token] = tokens.as_slice()
        && token.kind == crate::TokenKind::FatArrow
    {
        return Some(SyntaxValue::FatArrow(syntax.clone()));
    }
    if let Some((open, close, kind)) =
        tokens
            .first()
            .zip(tokens.last())
            .and_then(|(open, close)| match (open.kind, close.kind) {
                (crate::TokenKind::LParen, crate::TokenKind::RParen) => {
                    Some((open, close, DelimiterKind::Parenthesized))
                }
                (crate::TokenKind::LBracket, crate::TokenKind::RBracket) => {
                    Some((open, close, DelimiterKind::Bracketed))
                }
                (crate::TokenKind::LBrace, crate::TokenKind::RBrace) => {
                    Some((open, close, DelimiterKind::Braced))
                }
                _ => None,
            })
    {
        let _ = (open, close);
        let expected = MetaType::Delimited(
            kind,
            DelimitedMetaContents::Sequence(Box::new(MetaType::Syntax)),
        );
        return delimiter_argument_value(&expected, syntax, next_syntax_id);
    }
    if let Ok(expression) = crate::parser::parse_expression_fragment(syntax, next_syntax_id) {
        return Some(SyntaxValue::from_expression(expression));
    }
    if let Ok(item) = crate::parser::parse_item_fragment(syntax, next_syntax_id) {
        return Some(SyntaxValue::Item(Box::new(item)));
    }
    if let Ok(ty) = crate::parser::parse_type_fragment(syntax, false, next_syntax_id) {
        return Some(SyntaxValue::Type(ty));
    }
    crate::parser::parse_pattern_fragment(syntax, false, next_syntax_id)
        .ok()
        .map(SyntaxValue::Pattern)
}

fn skip_trivia(tokens: &[crate::SyntaxToken], mut cursor: usize, end: usize) -> usize {
    while cursor < end && tokens[cursor].kind.is_trivia() {
        cursor += 1;
    }
    cursor
}

fn candidate_ends(
    tokens: &[crate::SyntaxToken],
    cursor: usize,
    end: usize,
) -> impl Iterator<Item = usize> + '_ {
    (cursor + 1..=end).filter(|candidate| !tokens[candidate - 1].kind.is_trivia())
}

fn syntax_slice(parent: &Syntax, start: usize, end: usize) -> Syntax {
    let mut syntax = parent.clone();
    syntax.token_range = parent.token_range.start + start..parent.token_range.start + end;
    syntax
}

fn match_macro_arguments(
    definition: &MacroDefinition,
    arguments: &[&Expression],
    call_visibility: Option<&VisibilitySyntax>,
) -> Option<(Vec<MacroArgument>, usize)> {
    let mut matched = Vec::with_capacity(definition.parameters.len());
    let mut parameter_index = 0;
    let mut argument_index = 0;

    if definition.parameters.first() == Some(&MetaType::MacroCallVisibility) {
        matched.push(MacroArgument::Visibility(
            call_visibility.cloned().unwrap_or_else(private_visibility),
        ));
        parameter_index += 1;
    } else if call_visibility.is_some() {
        return None;
    }

    for expected in &definition.parameters[parameter_index..] {
        match expected {
            MetaType::Visibility => {
                if let Some(Expression::VisibilityArgument(visibility)) =
                    arguments.get(argument_index).copied()
                {
                    matched.push(MacroArgument::Visibility(visibility.clone()));
                    argument_index += 1;
                } else {
                    matched.push(MacroArgument::Visibility(private_visibility()));
                }
            }
            MetaType::MacroCallVisibility => return None,
            _ => {
                let argument = arguments.get(argument_index).copied()?;
                if !meta_type_matches(expected, argument) {
                    return None;
                }
                matched.push(MacroArgument::Expression(argument.clone()));
                argument_index += 1;
            }
        }
    }
    Some((matched, argument_index))
}

fn private_visibility() -> VisibilitySyntax {
    VisibilitySyntax {
        syntax: Syntax::compiler(),
        kind: VisibilityKind::Private,
    }
}

fn modifier_argument_matches(expected: &MetaType, argument: &ModifierArgument) -> bool {
    match expected {
        MetaType::Type => {
            let mut next_syntax_id = 0;
            crate::parser::parse_type_fragment(&argument.syntax, true, &mut next_syntax_id).is_ok()
        }
        MetaType::Pattern => {
            let mut next_syntax_id = 0;
            crate::parser::parse_pattern_fragment(&argument.syntax, true, &mut next_syntax_id)
                .is_ok()
        }
        MetaType::Item
        | MetaType::Syntax
        | MetaType::Visibility
        | MetaType::MacroCallVisibility => false,
        _ => argument
            .expression
            .as_ref()
            .is_some_and(|expression| meta_type_matches(expected, expression)),
    }
}

fn category_argument_syntax(argument: &Expression) -> Option<(&Syntax, bool)> {
    match argument {
        Expression::SyntaxArgument(argument) => Some((&argument.syntax, true)),
        Expression::Product(product) => Some((&product.syntax, true)),
        Expression::Name(name) => Some((&name.syntax, false)),
        Expression::Access(access) => Some((&access.syntax, false)),
        Expression::String(string) => Some((&string.syntax, false)),
        _ => None,
    }
}

fn meta_argument_expression<'a>(expected: &MetaType, argument: &'a Expression) -> &'a Expression {
    if matches!(expected, MetaType::Syntax | MetaType::Expr) {
        return argument;
    }
    let mut argument = argument;
    while let Expression::Product(product) = argument
        && let [element] = product.elements.as_slice()
        && element.name.is_none()
        && !element.spread
    {
        argument = &element.value;
    }
    argument
}

fn meta_type_at_least_as_specific(left: &MetaType, right: &MetaType) -> bool {
    left == right
        || matches!(right, MetaType::Syntax)
        || matches!(left, MetaType::Delimited(_, _))
            && matches!(right, MetaType::Expr | MetaType::Type | MetaType::Pattern)
        || match (left, right) {
            (MetaType::Product(left), MetaType::Product(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| meta_type_at_least_as_specific(left, right))
            }
            (MetaType::Optional(left), MetaType::Optional(right))
            | (MetaType::Sequence(left), MetaType::Sequence(right)) => {
                meta_type_at_least_as_specific(left, right)
            }
            (
                MetaType::Delimited(left_kind, left_contents),
                MetaType::Delimited(right_kind, right_contents),
            ) if left_kind == right_kind => {
                contents_at_least_as_specific(left_contents, right_contents)
            }
            _ => false,
        }
        || matches!(
            (left, right),
            (MetaType::Ident(_), MetaType::Expr)
                | (MetaType::CallExpr, MetaType::Expr)
                | (MetaType::UnstructuredExpr, MetaType::Expr)
                | (MetaType::Ident(Some(_)), MetaType::Ident(None))
                | (MetaType::MacroCallVisibility, MetaType::Visibility)
        )
}

fn contents_at_least_as_specific(
    left: &DelimitedMetaContents,
    right: &DelimitedMetaContents,
) -> bool {
    match (left, right) {
        (DelimitedMetaContents::Fixed(left), DelimitedMetaContents::Fixed(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| meta_type_at_least_as_specific(left, right))
        }
        (DelimitedMetaContents::Sequence(left), DelimitedMetaContents::Sequence(right)) => {
            meta_type_at_least_as_specific(left, right)
        }
        (DelimitedMetaContents::Fixed(left), DelimitedMetaContents::Sequence(right)) => left
            .iter()
            .all(|left| meta_type_at_least_as_specific(left, right)),
        (DelimitedMetaContents::Sequence(_), DelimitedMetaContents::Fixed(_)) => false,
        (
            DelimitedMetaContents::Separated {
                element: left_element,
                separator: left_separator,
            },
            DelimitedMetaContents::Separated {
                element: right_element,
                separator: right_separator,
            },
        ) => {
            meta_type_at_least_as_specific(left_element, right_element)
                && meta_type_at_least_as_specific(left_separator, right_separator)
        }
        (
            DelimitedMetaContents::Separated { element, separator },
            DelimitedMetaContents::Sequence(right),
        ) => {
            meta_type_at_least_as_specific(element, right)
                && meta_type_at_least_as_specific(separator, right)
        }
        (
            DelimitedMetaContents::Fixed(left),
            DelimitedMetaContents::Separated { element, separator },
        ) => fixed_matches_separated(left, element, separator),
        (DelimitedMetaContents::Sequence(_), DelimitedMetaContents::Separated { .. })
        | (DelimitedMetaContents::Separated { .. }, DelimitedMetaContents::Fixed(_)) => false,
    }
}

fn fixed_matches_separated(left: &[MetaType], element: &MetaType, separator: &MetaType) -> bool {
    left.iter().enumerate().all(|(index, value)| {
        meta_type_at_least_as_specific(value, if index % 2 == 0 { element } else { separator })
    })
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
            MetaType::CallExpr => "CallExpr".to_owned(),
            MetaType::UnstructuredExpr => "UnstructuredExpr".to_owned(),
            MetaType::Type => "Type".to_owned(),
            MetaType::Pattern => "Pattern".to_owned(),
            MetaType::Item => "Item".to_owned(),
            MetaType::Visibility => "Visibility".to_owned(),
            MetaType::MacroCallVisibility => "MacroCallVisibility".to_owned(),
            MetaType::Comma => "Comma".to_owned(),
            MetaType::Equals => "Equals".to_owned(),
            MetaType::FatArrow => "FatArrow".to_owned(),
            MetaType::Product(_) | MetaType::Optional(_) | MetaType::Sequence(_) => {
                format_meta_type(parameter)
            }
            MetaType::Delimited(_, _) => format_meta_type(parameter),
        })
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn resolved_macro(definition: &MacroDefinition) -> ResolvedMacro {
    let parameters = format_meta_signature(&definition.parameters);
    let result = format_meta_type(&definition.result);
    let signature = if parameters.is_empty() {
        result
    } else {
        format!("{parameters} -> {result}")
    };
    ResolvedMacro {
        declaration: definition.declaration.syntax.id,
        name: definition.key.name.clone(),
        modifier: definition.key.modifier,
        signature,
    }
}

fn format_meta_type(meta: &MetaType) -> String {
    match meta {
        MetaType::Syntax => "Syntax".to_owned(),
        MetaType::Expr => "Expr".to_owned(),
        MetaType::Ident(None) => "Ident String".to_owned(),
        MetaType::Ident(Some(spelling)) => format!("Ident {spelling:?}"),
        MetaType::CallExpr => "CallExpr".to_owned(),
        MetaType::UnstructuredExpr => "UnstructuredExpr".to_owned(),
        MetaType::Type => "Type".to_owned(),
        MetaType::Pattern => "Pattern".to_owned(),
        MetaType::Item => "Item".to_owned(),
        MetaType::Visibility => "Visibility".to_owned(),
        MetaType::MacroCallVisibility => "MacroCallVisibility".to_owned(),
        MetaType::Comma => "Comma".to_owned(),
        MetaType::Equals => "Equals".to_owned(),
        MetaType::FatArrow => "FatArrow".to_owned(),
        MetaType::Product(elements) => format!(
            "({})",
            elements
                .iter()
                .map(format_meta_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MetaType::Optional(element) => format!("Optional {}", format_meta_type(element)),
        MetaType::Sequence(element) => format!("Sequence {}", format_meta_type(element)),
        MetaType::Delimited(kind, contents) => {
            let name = match kind {
                DelimiterKind::Parenthesized => "Parenthesized",
                DelimiterKind::Bracketed => "Bracketed",
                DelimiterKind::Braced => "Braced",
            };
            let contents = match contents {
                DelimitedMetaContents::Fixed(elements) => format!(
                    "({})",
                    elements
                        .iter()
                        .map(format_meta_type)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                DelimitedMetaContents::Sequence(element) => {
                    format!("(Sequence {})", format_meta_type(element))
                }
                DelimitedMetaContents::Separated { element, separator } => format!(
                    "(Separated ({}) {})",
                    format_meta_type(element),
                    format_meta_type(separator)
                ),
            };
            format!("{name} {contents}")
        }
    }
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
        Pattern::Wildcard(wildcard) => match &wildcard.ty {
            Type::Inferred(_) => Some(MetaType::Syntax),
            ty => meta_type(ty),
        },
        Pattern::Product(_)
        | Pattern::Nominal(_)
        | Pattern::StringLiteral(_)
        | Pattern::Splice(_) => None,
    }
}

fn type_contains_syntax(ty: &Type) -> bool {
    match ty {
        Type::Named(named) => matches!(
            named.name.as_str(),
            "Syntax"
                | "Expr"
                | "Ident"
                | "CallExpr"
                | "Sequence"
                | "Optional"
                | "Separated"
                | "Comma"
                | "Equals"
                | "FatArrow"
                | "Parenthesized"
                | "Bracketed"
                | "Braced"
                | "UnstructuredExpr"
                | "Type"
                | "Pattern"
                | "Item"
                | "Visibility"
                | "MacroCallVisibility"
                | "Private"
                | "Public"
                | "PublicRepr"
        ),
        Type::Function(function) => {
            type_contains_syntax(&function.parameter)
                || function
                    .resources
                    .resources
                    .iter()
                    .any(type_contains_syntax)
                || type_contains_syntax(&function.result)
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
        Type::Inferred(_) | Type::StringLiteral(_) | Type::Splice(_) => false,
    }
}

fn type_contains_unshadowed_syntax(ty: &Type, declared: &std::collections::HashSet<&str>) -> bool {
    match ty {
        Type::Named(named)
            if named.namespace.is_none() && declared.contains(named.name.as_str()) =>
        {
            false
        }
        Type::Named(_) | Type::StringLiteral(_) | Type::Inferred(_) | Type::Splice(_) => {
            type_contains_syntax(ty)
        }
        Type::Function(function) => {
            type_contains_unshadowed_syntax(&function.parameter, declared)
                || function
                    .resources
                    .resources
                    .iter()
                    .any(|ty| type_contains_unshadowed_syntax(ty, declared))
                || type_contains_unshadowed_syntax(&function.result, declared)
        }
        Type::Product(product) => product
            .elements
            .iter()
            .any(|element| type_contains_unshadowed_syntax(&element.ty, declared)),
        Type::Sum(sum) => sum
            .alternatives
            .iter()
            .any(|ty| type_contains_unshadowed_syntax(ty, declared)),
        Type::Application(application) => {
            type_contains_unshadowed_syntax(&application.callee, declared)
                || type_contains_unshadowed_syntax(&application.argument, declared)
        }
        Type::Repeated(repeated) => type_contains_unshadowed_syntax(&repeated.element, declared),
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

fn binding_uses_macro_call_visibility(binding: &Binding) -> bool {
    binding
        .annotation
        .as_ref()
        .is_some_and(|annotation| type_contains_named(annotation, "MacroCallVisibility"))
        || binding.value.as_ref().is_some_and(|value| {
            let mut current = value;
            while let Expression::Function(function) = current {
                let contains = match &function.pattern {
                    Pattern::Binding(binding) => {
                        type_contains_named(&binding.ty, "MacroCallVisibility")
                    }
                    Pattern::Wildcard(wildcard) => {
                        type_contains_named(&wildcard.ty, "MacroCallVisibility")
                    }
                    _ => false,
                };
                if contains {
                    return true;
                }
                current = &function.body;
            }
            false
        })
}

fn type_contains_named(ty: &Type, expected: &str) -> bool {
    match ty {
        Type::Named(named) => named.namespace.is_none() && named.name == expected,
        Type::Function(function) => {
            type_contains_named(&function.parameter, expected)
                || function
                    .resources
                    .resources
                    .iter()
                    .any(|resource| type_contains_named(resource, expected))
                || type_contains_named(&function.result, expected)
        }
        Type::Product(product) => product
            .elements
            .iter()
            .any(|element| type_contains_named(&element.ty, expected)),
        Type::Sum(sum) => sum
            .alternatives
            .iter()
            .any(|alternative| type_contains_named(alternative, expected)),
        Type::Application(application) => {
            type_contains_named(&application.callee, expected)
                || type_contains_named(&application.argument, expected)
        }
        Type::Repeated(repeated) => type_contains_named(&repeated.element, expected),
        Type::Inferred(_) | Type::StringLiteral(_) | Type::Splice(_) => false,
    }
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
        Pattern::Wildcard(wildcard) => type_contains_syntax(&wildcard.ty),
        Pattern::StringLiteral(_) | Pattern::Splice(_) => false,
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
        Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
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
        Expression::Resource(_) | Expression::With(_) => true,
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

fn directly_evaluates_resource(expression: &Expression, arity: usize) -> bool {
    if arity > 0 {
        return match expression {
            Expression::Function(function) => {
                directly_evaluates_resource(&function.body, arity - 1)
            }
            Expression::Satisfies(satisfies) => {
                directly_evaluates_resource(&satisfies.value, arity)
            }
            _ => false,
        };
    }
    match expression {
        Expression::Resource(_) | Expression::With(_) => true,
        Expression::Satisfies(satisfies) => directly_evaluates_resource(&satisfies.value, 0),
        _ => false,
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
    if tokens.clone().next().is_none() {
        return name.syntax.definition_module().is_some();
    }
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
        Pattern::Wildcard(pattern) => matches_pattern_type(&pattern.ty, &value),
        Pattern::StringLiteral(pattern) => {
            let Value::String(value) = value else {
                return false;
            };
            crate::string_literal::decode(&pattern.literal).is_ok_and(|literal| literal == value)
        }
        Pattern::Binding(binding) => {
            if let Value::Syntax(SyntaxValue::Visibility(visibility)) = &value
                && matches!(binding.name.as_str(), "Private" | "Public" | "PublicRepr")
            {
                return visibility_pattern_matches(&binding.name, visibility.kind);
            }
            if binding.name == "Comma" && matches!(value, Value::Syntax(SyntaxValue::Comma(_))) {
                return true;
            }
            if binding.name == "Equals" && matches!(value, Value::Syntax(SyntaxValue::Equals(_))) {
                return true;
            }
            if binding.name == "FatArrow"
                && matches!(value, Value::Syntax(SyntaxValue::FatArrow(_)))
            {
                return true;
            }
            if binding.name.chars().next().is_some_and(char::is_uppercase)
                && let Value::Nominal(name, argument) = &value
            {
                return binding.name == *name
                    && matches!(argument.as_ref(), Value::Product(values) if values.is_empty());
            }
            if !matches_pattern_type(&binding.ty, &value) {
                return false;
            }
            environment.insert(
                binding.name.clone(),
                EnvironmentBinding::new(value, binding.mutable),
            );
            true
        }
        Pattern::Product(product) => {
            let values = match value {
                Value::Product(values) => values,
                Value::Sequence(values) => values.into_iter().map(|value| (None, value)).collect(),
                _ => return false,
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
                && let Value::Syntax(SyntaxValue::Ident(name)) = &value
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::String(name.name.clone()),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && pattern.name == "CallExpr"
                && let Value::Syntax(SyntaxValue::Call(call)) = value
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(vec![
                        (
                            Some("callee".to_owned()),
                            Value::Syntax(SyntaxValue::from_expression(*call.callee)),
                        ),
                        (
                            Some("argument".to_owned()),
                            Value::Syntax(SyntaxValue::from_expression(*call.argument)),
                        ),
                    ]),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && let Some(kind) = delimiter_kind(&pattern.name)
                && let Value::Syntax(SyntaxValue::Delimited(delimited)) = &value
                && delimited.kind == kind
            {
                let contents = match &delimited.contents {
                    DelimitedValueContents::Fixed(values) => Value::Product(values.clone()),
                    DelimitedValueContents::Sequence(values) => Value::Sequence(values.clone()),
                    DelimitedValueContents::Separated {
                        elements,
                        separator,
                        trailing,
                    } => Value::Separated {
                        elements: elements.clone(),
                        separator: separator.clone(),
                        trailing: *trailing,
                    },
                };
                let argument = match pattern.argument.as_ref() {
                    Pattern::Product(product) if product.elements.len() == 1 => {
                        &product.elements[0]
                    }
                    argument => argument,
                };
                return bind_pattern(argument, contents, environment);
            }
            if pattern.namespace.is_none()
                && pattern.name == "Sequence"
                && let Value::Sequence(values) = value
            {
                if let Pattern::Product(product) = pattern.argument.as_ref()
                    && let [Pattern::Binding(first), Pattern::Binding(rest)] =
                        product.elements.as_slice()
                    && first.name == "first"
                    && rest.name == "rest"
                {
                    let Some((head, tail)) = values.split_first() else {
                        return false;
                    };
                    return bind_pattern(
                        &pattern.argument,
                        Value::Product(vec![
                            (Some("first".to_owned()), head.clone()),
                            (Some("rest".to_owned()), Value::Sequence(tail.to_vec())),
                        ]),
                        environment,
                    );
                }
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(values.into_iter().map(|value| (None, value)).collect()),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && pattern.name == "Separated"
                && let Value::Separated {
                    elements,
                    separator,
                    trailing,
                } = value
            {
                let matched = bind_pattern(
                    &pattern.argument,
                    Value::Product(vec![
                        (Some("separator".to_owned()), *separator),
                        (Some("elements".to_owned()), Value::Sequence(elements)),
                        (Some("trailing".to_owned()), bool_value(trailing)),
                    ]),
                    environment,
                );
                return matched;
            }
            if pattern.namespace.is_none()
                && pattern.name == "Comma"
                && matches!(value, Value::Syntax(SyntaxValue::Comma(_)))
            {
                return bind_pattern(&pattern.argument, Value::Product(Vec::new()), environment);
            }
            if pattern.namespace.is_none()
                && pattern.name == "Equals"
                && matches!(value, Value::Syntax(SyntaxValue::Equals(_)))
            {
                return bind_pattern(&pattern.argument, Value::Product(Vec::new()), environment);
            }
            if pattern.namespace.is_none()
                && pattern.name == "FatArrow"
                && matches!(value, Value::Syntax(SyntaxValue::FatArrow(_)))
            {
                return bind_pattern(&pattern.argument, Value::Product(Vec::new()), environment);
            }
            if pattern.namespace.is_none()
                && let Value::Syntax(SyntaxValue::Visibility(visibility)) = &value
                && visibility_pattern_matches(&pattern.name, visibility.kind)
            {
                return bind_pattern(&pattern.argument, Value::Product(Vec::new()), environment);
            }
            let Value::Nominal(name, value) = value else {
                return false;
            };
            pattern.name == name && bind_pattern(&pattern.argument, *value, environment)
        }
        Pattern::Splice(_) => false,
    }
}

fn visibility_pattern_matches(name: &str, kind: VisibilityKind) -> bool {
    matches!(
        (name, kind),
        ("Private", VisibilityKind::Private)
            | ("Public", VisibilityKind::Public)
            | ("PublicRepr", VisibilityKind::PublicRepr)
    )
}

fn matches_pattern_type(ty: &Type, value: &Value) -> bool {
    match ty {
        Type::Inferred(_) => true,
        ty => meta_type(ty).is_none_or(|expected| meta_type_matches_value(&expected, value)),
    }
}

fn meta_type_matches_value(expected: &MetaType, value: &Value) -> bool {
    match (expected, value) {
        (MetaType::Syntax, Value::Syntax(_)) => true,
        (MetaType::Expr, Value::Syntax(syntax)) => syntax.to_expression().is_some(),
        (MetaType::Ident(spelling), Value::Syntax(SyntaxValue::Ident(name))) => spelling
            .as_ref()
            .is_none_or(|expected| expected == &name.name),
        (MetaType::CallExpr, Value::Syntax(SyntaxValue::Call(_))) => true,
        (MetaType::UnstructuredExpr, Value::Syntax(SyntaxValue::Unstructured(_))) => true,
        (MetaType::Type, Value::Syntax(SyntaxValue::Type(_))) => true,
        (MetaType::Pattern, Value::Syntax(SyntaxValue::Pattern(_))) => true,
        (MetaType::Item, Value::Syntax(SyntaxValue::Item(_))) => true,
        (MetaType::Comma, Value::Syntax(SyntaxValue::Comma(_))) => true,
        (MetaType::Equals, Value::Syntax(SyntaxValue::Equals(_))) => true,
        (MetaType::FatArrow, Value::Syntax(SyntaxValue::FatArrow(_))) => true,
        (MetaType::Product(expected), Value::Product(values)) => {
            expected.len() == values.len()
                && expected
                    .iter()
                    .zip(values)
                    .all(|(expected, (_, value))| meta_type_matches_value(expected, value))
        }
        (MetaType::Optional(expected), Value::Nominal(name, value)) => match name.as_str() {
            "None" => matches!(value.as_ref(), Value::Product(values) if values.is_empty()),
            "Some" => meta_type_matches_value(expected, value),
            _ => false,
        },
        (MetaType::Sequence(expected), Value::Sequence(values)) => values
            .iter()
            .all(|value| meta_type_matches_value(expected, value)),
        (
            MetaType::Delimited(expected_kind, expected_contents),
            Value::Syntax(SyntaxValue::Delimited(value)),
        ) => {
            value.kind == *expected_kind
                && match (&value.contents, expected_contents) {
                    (
                        DelimitedValueContents::Fixed(values),
                        DelimitedMetaContents::Fixed(expected),
                    ) => {
                        values.len() == expected.len()
                            && values.iter().zip(expected).all(|((_, value), expected)| {
                                meta_type_matches_value(expected, value)
                            })
                    }
                    (
                        DelimitedValueContents::Sequence(values),
                        DelimitedMetaContents::Sequence(expected),
                    ) => values
                        .iter()
                        .all(|value| meta_type_matches_value(expected, value)),
                    (
                        DelimitedValueContents::Separated {
                            elements,
                            separator,
                            ..
                        },
                        DelimitedMetaContents::Separated {
                            element,
                            separator: expected_separator,
                        },
                    ) => {
                        elements
                            .iter()
                            .all(|value| meta_type_matches_value(element, value))
                            && meta_type_matches_value(expected_separator, separator)
                    }
                    _ => false,
                }
        }
        (
            MetaType::Visibility | MetaType::MacroCallVisibility,
            Value::Syntax(SyntaxValue::Visibility(_)),
        ) => true,
        _ => false,
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
        return match environment.get(&splice.name).map(EnvironmentBinding::get) {
            Some(Value::Syntax(expression)) => match expression.clone().into_expression() {
                Some(expression) => Some(expression),
                None => {
                    diagnostics.push(Diagnostic::new(
                        splice.syntax.span.clone(),
                        format!(
                            "splice `${}` contains {} syntax, not expression syntax",
                            splice.name,
                            syntax_category(&expression)
                        ),
                    ));
                    None
                }
            },
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
            substitute_pattern(&mut function.pattern, environment, diagnostics)?;
            *function.body = substitute_splices(&function.body, environment, diagnostics)?
        }
        Expression::Satisfies(satisfies) => {
            *satisfies.value = substitute_splices(&satisfies.value, environment, diagnostics)?;
            substitute_type(&mut satisfies.ty, environment, diagnostics)?;
        }
        Expression::Match(match_) => {
            *match_.subject = substitute_splices(&match_.subject, environment, diagnostics)?;
            for arm in &mut match_.arms {
                substitute_pattern(&mut arm.pattern, environment, diagnostics)?;
                arm.body = substitute_splices(&arm.body, environment, diagnostics)?;
            }
        }
        Expression::Loop(loop_) => {
            for statement in &mut loop_.body.statements {
                substitute_statement(statement, environment, diagnostics)?;
            }
        }
        Expression::Resource(resource) => {
            substitute_type(&mut resource.resource, environment, diagnostics)?;
        }
        Expression::With(with) => {
            substitute_type(&mut with.resource, environment, diagnostics)?;
            *with.value = substitute_splices(&with.value, environment, diagnostics)?;
            for statement in &mut with.body.statements {
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
        Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
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
            substitute_binding(binding, environment, diagnostics)?;
        }
        Statement::PatternBinding(binding) => {
            substitute_pattern(&mut binding.pattern, environment, diagnostics)?;
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

fn item_output_supported(item: &Item) -> bool {
    match item {
        Item::Modified(modified) => item_output_supported(&modified.item),
        Item::VisibilitySplice(splice) => item_output_supported(&splice.item),
        Item::RepeatedItemSplice(_) => true,
        Item::VisibilityMacroInvocation(_) => true,
        Item::ExternBlock(_)
        | Item::TypeDeclaration(_)
        | Item::TraitDeclaration(_)
        | Item::TraitImplementation(_)
        | Item::Statement(_) => true,
        Item::Submodule(_) | Item::UseDeclaration(_) => true,
        Item::MacroDeclaration(_) => false,
    }
}

fn modifier_target_supported(item: &Item) -> bool {
    match item {
        Item::Modified(modified) => modifier_target_supported(&modified.item),
        Item::VisibilityMacroInvocation(_) => true,
        Item::VisibilitySplice(splice) => modifier_target_supported(&splice.item),
        Item::RepeatedItemSplice(_) => false,
        Item::ExternBlock(_)
        | Item::TypeDeclaration(_)
        | Item::TraitDeclaration(_)
        | Item::TraitImplementation(_) => true,
        Item::Statement(statement) => matches!(
            statement.as_ref(),
            Statement::Binding(_) | Statement::PatternBinding(_)
        ),
        Item::UseDeclaration(_) | Item::Submodule(_) | Item::MacroDeclaration(_) => false,
    }
}

fn item_syntax(item: &Item) -> &Syntax {
    match item {
        Item::Modified(value) => &value.syntax,
        Item::VisibilityMacroInvocation(value) => &value.syntax,
        Item::VisibilitySplice(value) => &value.syntax,
        Item::RepeatedItemSplice(value) => &value.syntax,
        Item::UseDeclaration(value) => &value.syntax,
        Item::Submodule(value) => &value.syntax,
        Item::ExternBlock(value) => &value.syntax,
        Item::TypeDeclaration(value) => &value.syntax,
        Item::MacroDeclaration(value) => &value.syntax,
        Item::TraitDeclaration(value) => &value.syntax,
        Item::TraitImplementation(value) => &value.syntax,
        Item::Statement(value) => statement_syntax(value),
    }
}

fn statement_syntax(statement: &Statement) -> &Syntax {
    match statement {
        Statement::Binding(value) => &value.syntax,
        Statement::PatternBinding(value) => &value.syntax,
        Statement::Assignment(value) => &value.syntax,
        Statement::Return(value) => &value.syntax,
        Statement::Break(value) => &value.syntax,
        Statement::Continue(value) => &value.syntax,
        Statement::Expression(value) => value.syntax(),
    }
}

fn substitute_binding(
    binding: &mut Binding,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    for bound in &mut binding.trait_bounds {
        substitute_trait_bound(bound, environment, diagnostics)?;
    }
    if let Some(annotation) = &mut binding.annotation {
        substitute_type(annotation, environment, diagnostics)?;
    }
    if let Some(value) = &mut binding.value {
        *value = substitute_splices(value, environment, diagnostics)?;
    }
    Some(())
}

fn substitute_trait_bound(
    bound: &mut crate::TraitBound,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    for argument in &mut bound.arguments {
        substitute_type(argument, environment, diagnostics)?;
    }
    Some(())
}

fn qualified_macro_access_path(
    access: &crate::AccessExpression,
) -> Option<(String, String, Option<usize>)> {
    fn collect(expression: &Expression, parts: &mut Vec<String>) -> Option<Option<usize>> {
        match expression {
            Expression::Name(name) => {
                parts.push(name.name.clone());
                Some(name.syntax.definition_module())
            }
            Expression::Access(access) => {
                let definition_module = collect(&access.value, parts)?;
                let Accessor::Name(name) = &access.accessor else {
                    return None;
                };
                parts.push(name.clone());
                Some(definition_module)
            }
            _ => None,
        }
    }

    let mut parts = Vec::new();
    let definition_module = collect(&access.value, &mut parts)?;
    let Accessor::Name(item) = &access.accessor else {
        return None;
    };
    parts.push(item.clone());
    let item = parts.pop()?;
    (!parts.is_empty()).then(|| (parts.join("."), item, definition_module))
}

fn substitute_type(
    ty: &mut Type,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    if let Type::Splice(splice) = ty {
        if splice.repeated {
            diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "repeated splices are not supported yet",
            ));
            return None;
        }
        return match environment.get(&splice.name).map(EnvironmentBinding::get) {
            Some(Value::Syntax(SyntaxValue::Type(value))) => {
                *ty = value;
                Some(())
            }
            Some(Value::Syntax(SyntaxValue::Ident(identifier))) => {
                *ty = Type::Named(crate::NamedType {
                    syntax: identifier.syntax,
                    namespace: None,
                    name: identifier.name,
                });
                Some(())
            }
            Some(Value::Syntax(SyntaxValue::Delimited(delimited)))
                if delimited.kind == DelimiterKind::Parenthesized =>
            {
                let values = match delimited.contents {
                    DelimitedValueContents::Fixed(values) => {
                        values.into_iter().map(|(_, value)| value).collect()
                    }
                    DelimitedValueContents::Sequence(values) => values,
                    DelimitedValueContents::Separated { .. } => {
                        diagnostics.push(Diagnostic::new(
                            splice.syntax.span.clone(),
                            "parenthesized type splice must contain identifiers",
                        ));
                        return None;
                    }
                };
                let elements = values
                    .into_iter()
                    .map(|value| {
                        let Value::Syntax(SyntaxValue::Ident(identifier)) = value else {
                            return None;
                        };
                        Some(crate::TypeElement {
                            syntax: identifier.syntax.clone(),
                            name: None,
                            ty: Type::Named(crate::NamedType {
                                syntax: identifier.syntax,
                                namespace: None,
                                name: identifier.name,
                            }),
                            spread: false,
                        })
                    })
                    .collect::<Option<Vec<_>>>();
                let Some(elements) = elements else {
                    diagnostics.push(Diagnostic::new(
                        splice.syntax.span.clone(),
                        "parenthesized type splice must contain identifiers",
                    ));
                    return None;
                };
                *ty = Type::Product(crate::ProductType {
                    syntax: delimited.syntax,
                    elements,
                    variadic: false,
                });
                Some(())
            }
            Some(Value::Syntax(value)) => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!(
                        "type splice `${}` contains {} syntax",
                        splice.name,
                        syntax_category(&value)
                    ),
                ));
                None
            }
            Some(_) => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!("type splice `${}` does not contain `Syntax`", splice.name),
                ));
                None
            }
            None => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!("unknown type splice `${}`", splice.name),
                ));
                None
            }
        };
    }
    match ty {
        Type::Product(product) => {
            for element in &mut product.elements {
                substitute_type(&mut element.ty, environment, diagnostics)?;
            }
        }
        Type::Sum(sum) => {
            for alternative in &mut sum.alternatives {
                substitute_type(alternative, environment, diagnostics)?;
            }
        }
        Type::Function(function) => {
            substitute_type(&mut function.parameter, environment, diagnostics)?;
            for resource in &mut function.resources.resources {
                substitute_type(resource, environment, diagnostics)?;
            }
            substitute_type(&mut function.result, environment, diagnostics)?;
        }
        Type::Application(application) => {
            substitute_type(&mut application.callee, environment, diagnostics)?;
            substitute_type(&mut application.argument, environment, diagnostics)?;
        }
        Type::Repeated(repeated) => {
            substitute_type(&mut repeated.element, environment, diagnostics)?
        }
        Type::Named(named) => {
            if let Some(namespace) = &mut named.namespace {
                substitute_identifier(namespace, environment, diagnostics)?;
            }
            substitute_identifier(&mut named.name, environment, diagnostics)?;
        }
        Type::Inferred(_) | Type::StringLiteral(_) => {}
        Type::Splice(_) => unreachable!(),
    }
    Some(())
}

fn substitute_pattern(
    pattern: &mut Pattern,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    if let Pattern::Splice(splice) = pattern {
        if splice.repeated {
            diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                "repeated splices are not supported yet",
            ));
            return None;
        }
        return match environment.get(&splice.name).map(EnvironmentBinding::get) {
            Some(Value::Syntax(SyntaxValue::Pattern(value))) => {
                *pattern = value;
                Some(())
            }
            Some(Value::Syntax(value)) => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!(
                        "pattern splice `${}` contains {} syntax",
                        splice.name,
                        syntax_category(&value)
                    ),
                ));
                None
            }
            Some(_) => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!(
                        "pattern splice `${}` does not contain `Syntax`",
                        splice.name
                    ),
                ));
                None
            }
            None => {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!("unknown pattern splice `${}`", splice.name),
                ));
                None
            }
        };
    }
    match pattern {
        Pattern::Binding(binding) => substitute_type(&mut binding.ty, environment, diagnostics)?,
        Pattern::Wildcard(wildcard) => substitute_type(&mut wildcard.ty, environment, diagnostics)?,
        Pattern::Product(product) => {
            for element in &mut product.elements {
                substitute_pattern(element, environment, diagnostics)?;
            }
        }
        Pattern::Nominal(nominal) => {
            substitute_pattern(&mut nominal.argument, environment, diagnostics)?
        }
        Pattern::StringLiteral(_) => {}
        Pattern::Splice(_) => unreachable!(),
    }
    Some(())
}

fn substitute_item(
    item: &mut Item,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    if let Item::VisibilitySplice(splice) = item {
        let value = environment.get(&splice.name).map(EnvironmentBinding::get);
        let Some(Value::Syntax(SyntaxValue::Visibility(visibility))) = value else {
            diagnostics.push(Diagnostic::new(
                splice.syntax.span.clone(),
                match value {
                    Some(Value::Syntax(value)) => format!(
                        "visibility splice `${}` contains {} syntax",
                        splice.name,
                        syntax_category(&value)
                    ),
                    Some(_) => format!(
                        "visibility splice `${}` does not contain `Syntax`",
                        splice.name
                    ),
                    None => format!("unknown visibility splice `${}`", splice.name),
                },
            ));
            return None;
        };
        let mut replacement = (*splice.item).clone();
        apply_visibility_to_item(
            &mut replacement,
            visibility.kind,
            splice.syntax.span.clone(),
            diagnostics,
        )?;
        substitute_item(&mut replacement, environment, diagnostics)?;
        *item = replacement;
        return Some(());
    }
    match item {
        Item::Modified(modified) => {
            for modifier in &mut modified.modifiers {
                if let Some(argument) = &mut modifier.argument
                    && let Some(expression) = &mut argument.expression
                {
                    *expression = substitute_splices(expression, environment, diagnostics)?;
                }
            }
            substitute_item(&mut modified.item, environment, diagnostics)?;
        }
        Item::VisibilityMacroInvocation(invocation) => {
            invocation.expression =
                substitute_splices(&invocation.expression, environment, diagnostics)?;
        }
        Item::VisibilitySplice(_) => unreachable!(),
        Item::RepeatedItemSplice(_) => unreachable!("repeated item splices expand as lists"),
        Item::ExternBlock(block) => {
            for binding in &mut block.bindings {
                substitute_binding(binding, environment, diagnostics)?;
            }
        }
        Item::TraitDeclaration(declaration) => {
            for prerequisite in &mut declaration.prerequisites {
                substitute_trait_bound(prerequisite, environment, diagnostics)?;
            }
            for member in &mut declaration.members {
                substitute_type(&mut member.annotation, environment, diagnostics)?;
                if let Some(default) = &mut member.default {
                    *default = substitute_splices(default, environment, diagnostics)?;
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            for argument in &mut implementation.arguments {
                substitute_type(argument, environment, diagnostics)?;
            }
            for member in &mut implementation.members {
                member.value = substitute_splices(&member.value, environment, diagnostics)?;
            }
        }
        Item::Statement(statement) => {
            substitute_statement(statement, environment, diagnostics)?;
        }
        Item::TypeDeclaration(declaration) => {
            substitute_identifier(&mut declaration.name, environment, diagnostics)?;
            substitute_type_parameter_list(
                &mut declaration.type_parameters,
                environment,
                diagnostics,
            )?;
            for bound in &mut declaration.trait_bounds {
                substitute_trait_bound(bound, environment, diagnostics)?;
            }
            if let Some(underlying) = &mut declaration.underlying {
                substitute_type(underlying, environment, diagnostics)?;
            }
        }
        Item::Submodule(submodule) => {
            substitute_identifier(&mut submodule.name, environment, diagnostics)?;
            substitute_item_list(&mut submodule.module.items, environment, diagnostics)?;
        }
        Item::UseDeclaration(declaration) => {
            for component in &mut declaration.path {
                substitute_identifier(component, environment, diagnostics)?;
            }
        }
        Item::MacroDeclaration(_) => {
            unreachable!("unsupported item output must be rejected before substitution")
        }
    }
    Some(())
}

fn apply_visibility_to_item(
    item: &mut Item,
    kind: VisibilityKind,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    if let Item::Modified(modified) = item {
        return apply_visibility_to_item(&mut modified.item, kind, span, diagnostics);
    }
    let public = if kind == VisibilityKind::Private {
        Visibility::Private
    } else {
        Visibility::Public
    };
    match item {
        Item::ExternBlock(block) if kind != VisibilityKind::PublicRepr => block.visibility = public,
        Item::Submodule(submodule) => submodule.visibility = public,
        Item::UseDeclaration(declaration) if kind != VisibilityKind::PublicRepr => {
            declaration.visibility = public
        }
        Item::TypeDeclaration(declaration) => {
            declaration.visibility = public;
            declaration.representation_visibility = if kind == VisibilityKind::PublicRepr {
                Visibility::Public
            } else {
                Visibility::Private
            };
            if kind == VisibilityKind::PublicRepr
                && !matches!(
                    declaration.kind,
                    crate::TypeDeclarationKind::Distinct | crate::TypeDeclarationKind::Singleton
                )
            {
                diagnostics.push(Diagnostic::new(
                    span,
                    "`PublicRepr` visibility requires a represented distinct type",
                ));
                return None;
            }
        }
        Item::TraitDeclaration(declaration) if kind != VisibilityKind::PublicRepr => {
            declaration.visibility = public
        }
        Item::Statement(statement) => {
            let Statement::Binding(binding) = statement.as_mut() else {
                diagnostics.push(Diagnostic::new(
                    span,
                    "visibility may only be spliced onto a declaration item",
                ));
                return None;
            };
            if kind == VisibilityKind::PublicRepr {
                diagnostics.push(Diagnostic::new(
                    span,
                    "`PublicRepr` visibility may only be applied to a represented distinct type",
                ));
                return None;
            }
            binding.visibility = public;
        }
        _ => {
            diagnostics.push(Diagnostic::new(
                span,
                if kind == VisibilityKind::PublicRepr {
                    "`PublicRepr` visibility may only be applied to a represented distinct type"
                } else {
                    "visibility may only be spliced onto `let`, `def`, `type`, `extern`, or `trait` declarations"
                },
            ));
            return None;
        }
    }
    Some(())
}

fn substitute_item_list(
    items: &mut Vec<Item>,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    let mut substituted = Vec::new();
    for mut item in std::mem::take(items) {
        if let Item::RepeatedItemSplice(splice) = item {
            let value = environment.get(&splice.name).map(EnvironmentBinding::get);
            let Some(Value::Sequence(values)) = value else {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span,
                    format!(
                        "repeated item splice `${}` requires `Sequence Item`",
                        splice.name
                    ),
                ));
                return None;
            };
            for value in values {
                let Value::Syntax(SyntaxValue::Item(item)) = value else {
                    diagnostics.push(Diagnostic::new(
                        splice.syntax.span.clone(),
                        format!(
                            "repeated item splice `${}` contains a non-item value",
                            splice.name
                        ),
                    ));
                    return None;
                };
                substituted.push(*item);
            }
        } else {
            substitute_item(&mut item, environment, diagnostics)?;
            substituted.push(item);
        }
    }
    *items = substituted;
    Some(())
}

fn substitute_type_parameter_list(
    parameters: &mut Vec<crate::TypeParameterPattern>,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    let mut substituted = Vec::new();
    for parameter in std::mem::take(parameters) {
        let crate::TypeParameterPattern::Splice(splice) = parameter else {
            substituted.push(parameter);
            continue;
        };
        let value = environment.get(&splice.name).map(EnvironmentBinding::get);
        let Some(Value::Sequence(values)) = value else {
            let actual = match value {
                None => "an unknown value",
                Some(Value::Product(_)) => "a product",
                Some(Value::Syntax(_)) => "syntax",
                Some(_) => "another compile-time value",
            };
            diagnostics.push(Diagnostic::new(
                splice.syntax.span,
                format!(
                    "type-parameter splice `${}...` requires `Sequence (Ident String)`, but contains {actual}",
                    splice.name,
                ),
            ));
            return None;
        };
        for value in values {
            let Value::Syntax(SyntaxValue::Ident(identifier)) = value else {
                diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    format!(
                        "type-parameter splice `${}...` contains a non-identifier value",
                        splice.name
                    ),
                ));
                return None;
            };
            substituted.push(crate::TypeParameterPattern::Binding(
                crate::TypeParameterBinding {
                    syntax: identifier.syntax,
                    name: identifier.name,
                    sized: true,
                },
            ));
        }
    }
    *parameters = substituted;
    Some(())
}

fn substitute_identifier(
    name: &mut String,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    let Some(splice) = name.strip_prefix('$') else {
        return Some(());
    };
    match environment.get(splice).map(EnvironmentBinding::get) {
        Some(Value::Syntax(SyntaxValue::Ident(identifier))) => {
            *name = identifier.name;
            Some(())
        }
        _ => {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("identifier splice `${splice}` requires `Ident String`"),
            ));
            None
        }
    }
}

fn alpha_rename_item(item: &mut Item, mark: u64) {
    let mut scopes = Vec::new();
    match item {
        Item::Modified(modified) => {
            for modifier in &mut modified.modifiers {
                if let Some(argument) = &mut modifier.argument
                    && let Some(expression) = &mut argument.expression
                {
                    alpha_rename_expression(expression, mark, &mut scopes);
                }
            }
            alpha_rename_item(&mut modified.item, mark);
        }
        Item::VisibilityMacroInvocation(invocation) => {
            alpha_rename_expression(&mut invocation.expression, mark, &mut scopes);
        }
        Item::VisibilitySplice(splice) => alpha_rename_item(&mut splice.item, mark),
        Item::RepeatedItemSplice(_) => {}
        Item::ExternBlock(block) => {
            for binding in &mut block.bindings {
                if let Some(value) = &mut binding.value {
                    alpha_rename_expression(value, mark, &mut scopes);
                }
            }
        }
        Item::TraitDeclaration(declaration) => {
            for member in &mut declaration.members {
                if let Some(default) = &mut member.default {
                    alpha_rename_expression(default, mark, &mut scopes);
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            for member in &mut implementation.members {
                alpha_rename_expression(&mut member.value, mark, &mut scopes);
            }
        }
        Item::Statement(statement) => match statement.as_mut() {
            Statement::Binding(binding) => {
                if let Some(value) = &mut binding.value {
                    alpha_rename_expression(value, mark, &mut scopes);
                }
            }
            Statement::PatternBinding(binding) => {
                alpha_rename_expression(&mut binding.value, mark, &mut scopes)
            }
            Statement::Assignment(assignment) => {
                alpha_rename_expression(&mut assignment.target, mark, &mut scopes);
                alpha_rename_expression(&mut assignment.value, mark, &mut scopes);
            }
            Statement::Return(return_) => {
                alpha_rename_expression(&mut return_.value, mark, &mut scopes)
            }
            Statement::Break(break_) => {
                if let Some(value) = &mut break_.value {
                    alpha_rename_expression(value, mark, &mut scopes);
                }
            }
            Statement::Continue(_) => {}
            Statement::Expression(expression) => {
                alpha_rename_expression(expression, mark, &mut scopes)
            }
        },
        Item::TypeDeclaration(_) => {}
        Item::Submodule(submodule) => {
            for item in &mut submodule.module.items {
                alpha_rename_item(item, mark);
            }
        }
        Item::UseDeclaration(_) => {}
        Item::MacroDeclaration(_) => {
            unreachable!("unsupported item output must be rejected before hygiene")
        }
    }
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
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
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
        Expression::Resource(_) => {}
        Expression::With(with) => {
            alpha_rename_expression(&mut with.value, mark, scopes);
            alpha_rename_block(&mut with.body, mark, scopes);
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
        Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
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
        Expression::Resource(value) => &mut value.syntax,
        Expression::With(value) => &mut value.syntax,
        Expression::Block(value) => &mut value.syntax,
        Expression::Product(value) => &mut value.syntax,
        Expression::Call(value) => &mut value.syntax,
        Expression::Access(value) => &mut value.syntax,
        Expression::Index(value) => &mut value.syntax,
        Expression::Infix(value) => &mut value.syntax,
        Expression::SyntaxArgument(value) => &mut value.syntax,
        Expression::VisibilityArgument(value) => &mut value.syntax,
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
        Pattern::Wildcard(wildcard) => {
            expander.freshen_syntax(&mut wildcard.syntax, module, mark);
            freshen_type(expander, &mut wildcard.ty, module, mark);
        }
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
        Pattern::Splice(splice) => expander.freshen_syntax(&mut splice.syntax, module, mark),
    }
}

fn freshen_statement(
    expander: &mut MacroExpander,
    statement: &mut Statement,
    module: ModuleId,
    mark: u64,
) {
    match statement {
        Statement::Binding(binding) => freshen_binding(expander, binding, module, mark),
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

fn freshen_binding(
    expander: &mut MacroExpander,
    binding: &mut Binding,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut binding.syntax, module, mark);
    for parameter in &mut binding.type_parameters {
        freshen_type_parameter(expander, parameter, module, mark);
    }
    for bound in &mut binding.trait_bounds {
        freshen_trait_bound(expander, bound, module, mark);
    }
    if let Some(annotation) = &mut binding.annotation {
        freshen_type(expander, annotation, module, mark);
    }
    if let Some(value) = &mut binding.value {
        expander.freshen_expression(value, module, mark);
    }
}

fn freshen_type_parameter(
    expander: &mut MacroExpander,
    parameter: &mut crate::TypeParameterPattern,
    module: ModuleId,
    mark: u64,
) {
    match parameter {
        crate::TypeParameterPattern::Binding(binding) => {
            expander.freshen_syntax(&mut binding.syntax, module, mark)
        }
        crate::TypeParameterPattern::Product(product) => {
            expander.freshen_syntax(&mut product.syntax, module, mark);
            for element in &mut product.elements {
                freshen_type_parameter(expander, element, module, mark);
            }
        }
        crate::TypeParameterPattern::Splice(splice) => {
            expander.freshen_syntax(&mut splice.syntax, module, mark);
        }
    }
}

fn freshen_trait_bound(
    expander: &mut MacroExpander,
    bound: &mut crate::TraitBound,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut bound.syntax, module, mark);
    expander.freshen_syntax(&mut bound.trait_name.syntax, module, mark);
    for argument in &mut bound.arguments {
        freshen_type(expander, argument, module, mark);
    }
}

fn freshen_item(expander: &mut MacroExpander, item: &mut Item, module: ModuleId, mark: u64) {
    match item {
        Item::Modified(modified) => {
            expander.freshen_syntax(&mut modified.syntax, module, mark);
            for modifier in &mut modified.modifiers {
                expander.freshen_syntax(&mut modifier.syntax, module, mark);
                if let Some(argument) = &mut modifier.argument {
                    expander.freshen_syntax(&mut argument.syntax, module, mark);
                    if let Some(expression) = &mut argument.expression {
                        expander.freshen_expression(expression, module, mark);
                    }
                }
            }
            freshen_item(expander, &mut modified.item, module, mark);
        }
        Item::VisibilityMacroInvocation(invocation) => {
            expander.freshen_syntax(&mut invocation.syntax, module, mark);
            expander.freshen_syntax(&mut invocation.visibility.syntax, module, mark);
            expander.freshen_expression(&mut invocation.expression, module, mark);
        }
        Item::VisibilitySplice(splice) => {
            expander.freshen_syntax(&mut splice.syntax, module, mark);
            freshen_item(expander, &mut splice.item, module, mark);
        }
        Item::RepeatedItemSplice(splice) => {
            expander.freshen_syntax(&mut splice.syntax, module, mark);
        }
        Item::ExternBlock(block) => {
            expander.freshen_syntax(&mut block.syntax, module, mark);
            for binding in &mut block.bindings {
                freshen_binding(expander, binding, module, mark);
            }
        }
        Item::TypeDeclaration(declaration) => {
            expander.freshen_syntax(&mut declaration.syntax, module, mark);
            for parameter in &mut declaration.type_parameters {
                freshen_type_parameter(expander, parameter, module, mark);
            }
            for bound in &mut declaration.trait_bounds {
                freshen_trait_bound(expander, bound, module, mark);
            }
            if let Some(underlying) = &mut declaration.underlying {
                freshen_type(expander, underlying, module, mark);
            }
        }
        Item::TraitDeclaration(declaration) => {
            expander.freshen_syntax(&mut declaration.syntax, module, mark);
            for parameter in &mut declaration.type_parameters {
                freshen_type_parameter(expander, parameter, module, mark);
            }
            for prerequisite in &mut declaration.prerequisites {
                freshen_trait_bound(expander, prerequisite, module, mark);
            }
            for member in &mut declaration.members {
                expander.freshen_syntax(&mut member.syntax, module, mark);
                freshen_type(expander, &mut member.annotation, module, mark);
                if let Some(default) = &mut member.default {
                    expander.freshen_expression(default, module, mark);
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            expander.freshen_syntax(&mut implementation.syntax, module, mark);
            expander.freshen_syntax(&mut implementation.trait_name.syntax, module, mark);
            for argument in &mut implementation.arguments {
                freshen_type(expander, argument, module, mark);
            }
            for member in &mut implementation.members {
                expander.freshen_syntax(&mut member.syntax, module, mark);
                expander.freshen_expression(&mut member.value, module, mark);
            }
        }
        Item::Statement(statement) => freshen_statement(expander, statement, module, mark),
        Item::Submodule(submodule) => {
            expander.freshen_syntax(&mut submodule.syntax, module, mark);
            expander.freshen_syntax(&mut submodule.module.syntax, module, mark);
            for item in &mut submodule.module.items {
                freshen_item(expander, item, module, mark);
            }
        }
        Item::UseDeclaration(declaration) => {
            expander.freshen_syntax(&mut declaration.syntax, module, mark);
        }
        Item::MacroDeclaration(_) => {
            unreachable!("unsupported item output must be rejected before freshening")
        }
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
        Type::Splice(ty) => &mut ty.syntax,
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
            expander.freshen_syntax(&mut function.resources.syntax, module, mark);
            for resource in &mut function.resources.resources {
                freshen_type(expander, resource, module, mark);
            }
            freshen_type(expander, &mut function.result, module, mark);
        }
        Type::Application(application) => {
            freshen_type(expander, &mut application.callee, module, mark);
            freshen_type(expander, &mut application.argument, module, mark);
        }
        Type::Repeated(repeated) => freshen_type(expander, &mut repeated.element, module, mark),
        Type::Inferred(_) | Type::StringLiteral(_) | Type::Named(_) | Type::Splice(_) => {}
    }
}
