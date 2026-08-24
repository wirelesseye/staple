use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use crate::{
    Accessor, Binding, BindingKind, BlockExpression, Diagnostic, Expression, Item, LogicalOperator,
    MacroDeclaration, ModifierArgument, ModifierInvocation, ModuleId, Pattern, Program,
    ResolvedMacro, Span, Syntax, SyntaxId, Type, UseDeclaration, UseKind, Visibility,
    VisibilityKind, VisibilitySyntax,
};

const MAX_EXPANSION_DEPTH: usize = 128;
const MAX_EVALUATION_STEPS: usize = 1_000_000;
/// Bounds `MacroExpander::helper_eval_depth`: nesting of "look up a named
/// binding and evaluate its raw initializer" or "apply a compile-time
/// function" calls, which is where a self-referential or mutually
/// recursive `def`/`const` (or one whose base case is unreachable) would
/// otherwise overflow the native stack — well before
/// `MAX_EVALUATION_STEPS` steps accumulate, since each nesting level costs
/// real Rust stack, not just a step count. Chosen with a comfortable
/// safety margin below the depth empirically observed to overflow the
/// stack of the dedicated 256 MiB thread `NameResolver::resolve_program`
/// spawns for this (unoptimized debug builds use dramatically more stack
/// per frame than release builds, so this is calibrated against the
/// worst case).
const MAX_HELPER_EVAL_DEPTH: usize = 150;

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
    ParseQuote,
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
    Sequence(Vec<Expression>),
}

#[derive(Clone)]
struct SelectedMacro {
    definition: MacroDefinition,
    arguments: Vec<MacroArgument>,
    consumed: usize,
    effective_parameters: Vec<(MetaType, bool)>,
}

enum ModifierChainResult {
    Item(Item),
    Items(Vec<Item>),
}

fn split_first_item(mut items: Vec<Item>) -> (Option<Item>, Vec<Item>) {
    if items.is_empty() {
        (None, items)
    } else {
        let first = items.remove(0);
        (Some(first), items)
    }
}

fn split_modifier_chain_result(result: ModifierChainResult) -> (Option<Item>, Vec<Item>) {
    match result {
        ModifierChainResult::Item(item) => (Some(item), Vec::new()),
        ModifierChainResult::Items(items) => split_first_item(items),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MetaType {
    Syntax,
    SyntaxNode,
    Expr,
    Ident(Option<String>),
    CallExpr,
    StringExpr,
    UnstructuredExpr,
    Type,
    Pattern,
    BindingPattern,
    NominalPattern,
    Item,
    TypeDeclarationItem,
    UnstructuredItem,
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
pub(crate) enum DelimiterKind {
    Parenthesized,
    Bracketed,
    Braced,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DelimitedMetaContents {
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
    Raw(Syntax),
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
            Self::Raw(_) => None,
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

    fn syntax(&self) -> Option<&Syntax> {
        match self {
            Self::Raw(syntax)
            | Self::Comma(syntax)
            | Self::Equals(syntax)
            | Self::FatArrow(syntax) => Some(syntax),
            Self::Ident(value) => Some(&value.syntax),
            Self::Call(value) => Some(&value.syntax),
            Self::Unstructured(value) => Some(value.syntax()),
            Self::Type(value) => Some(value.syntax()),
            Self::Pattern(value) => Some(value.syntax()),
            Self::Item(value) => Some(item_syntax(value)),
            Self::Visibility(value) => Some(&value.syntax),
            Self::Delimited(value) => Some(&value.syntax),
            Self::Items(_) => None,
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
                        designated: false,
                        value: expression,
                        spread: false,
                        named_spread: false,
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
                let items = values
                    .into_iter()
                    .map(|value| match value {
                        Value::Syntax(value) => value.into_expression().map(Item::Expression),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(Expression::Block(BlockExpression {
                    syntax: self.syntax,
                    items,
                }))
            }
        }
    }
}

fn syntax_category(value: &SyntaxValue) -> &'static str {
    match value {
        SyntaxValue::Raw(_) => "syntax fragment",
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
        quote_result: Option<MetaType>,
    },
    Helper(ModuleId, Binding),
    Product(Vec<(Option<String>, Value)>),
    Integer(i128),
    Float(f64),
    String(String),
    Nominal(String, Box<Value>),
    Sequence(Vec<Value>),
    Separated {
        elements: Vec<Value>,
        separator: Box<Value>,
        trailing: bool,
    },
}

/// Sets a named field within a compile-time product value, overriding an
/// existing field with the same name in place (keeping its original
/// position) or appending a new one.
fn set_named_product_field(values: &mut Vec<(Option<String>, Value)>, name: String, value: Value) {
    if let Some(existing) = values
        .iter_mut()
        .find(|(existing_name, _)| existing_name.as_deref() == Some(name.as_str()))
    {
        existing.1 = value;
    } else {
        values.push((Some(name), value));
    }
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
                designated: false,
                value: value.into_expression()?,
                spread: false,
                named_spread: false,
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

fn quote_contains_raw_splice(contents: &Syntax, environment: &Environment) -> bool {
    let tokens = contents.tokens();
    tokens.windows(2).any(|pair| {
        pair[0].kind == crate::TokenKind::Dollar
            && pair[1].kind == crate::TokenKind::Identifier
            && environment
                .get(&pair[1].text)
                .map(EnvironmentBinding::get)
                .is_some_and(|value| matches!(value, Value::Syntax(SyntaxValue::Raw(_))))
    })
}

pub(crate) struct MacroAnalysis {
    pub definitions: HashMap<SyntaxId, ResolvedMacro>,
    pub invocations: HashMap<SyntaxId, ResolvedMacro>,
    pub helpers: Vec<(ModuleId, Binding)>,
}

pub(crate) fn expand_program(
    mut program: Program,
) -> Result<(Program, MacroAnalysis), Vec<Diagnostic>> {
    let mut expander = MacroExpander::new(&program);
    expander.validate_definitions();
    for module in program.modules() {
        for item in &module.syntax.items {
            if let Item::Binding(binding) = item
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
                Item::Binding(binding) if binding_is_compile_time_helper(binding)
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
            if let Item::Binding(binding) = item
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
    imported_modules: HashMap<SyntaxId, ModuleId>,
    use_kinds: HashMap<SyntaxId, UseKind>,
    child_modules: HashMap<SyntaxId, ModuleId>,
    parent_modules: Vec<Option<ModuleId>>,
    /// Whether each module is a `companion` submodule. A companion is an
    /// extension of its declaring module's namespace (`Staple.md`'s "Type
    /// companions" section: "Its body therefore sees the parent's
    /// declarations without spelling `use super.*`"), so macro/name lookup
    /// for a companion falls back to its parent's scope — see the walk in
    /// `resolve_macro`.
    companion_modules: Vec<bool>,
    diagnostics: Vec<Diagnostic>,
    next_syntax_id: usize,
    next_mark: u64,
    steps: usize,
    expansion_stack: Vec<MacroKey>,
    emitted_items: Option<Vec<Item>>,
    invocations: HashMap<SyntaxId, ResolvedMacro>,
    quote_context: Option<MetaType>,
    /// Depth of nested "look up a named binding and evaluate its raw
    /// initializer from scratch" calls (see `eval_helper_value`) currently
    /// on the Rust call stack. Bounds a self-referential or mutually
    /// recursive `def`/`const` (e.g. `const x = x + 1`) to a diagnostic
    /// instead of a native stack overflow — `MAX_EVALUATION_STEPS`
    /// alone doesn't help here, since the crash happens well before a
    /// million steps accumulate.
    helper_eval_depth: usize,
}

fn provisional_use_kind(program: &Program, declaration: &UseDeclaration) -> UseKind {
    if declaration.kind != UseKind::Dotted {
        return declaration.kind.clone();
    }
    let Some(candidates) = program.dotted_import(declaration.syntax.id) else {
        return UseKind::Dotted;
    };
    if candidates.namespace.is_some() {
        UseKind::Namespace
    } else if candidates.item_module.is_some() {
        UseKind::Selected(vec![
            declaration
                .path
                .last()
                .expect("dotted import has a final component")
                .clone(),
        ])
    } else {
        UseKind::Dotted
    }
}

impl MacroExpander {
    fn new(program: &Program) -> Self {
        let mut definitions = HashMap::new();
        let mut scopes = vec![ModuleScope::default(); program.modules().len()];
        let core = program.standard_library_core();
        let syntax = program.standard_library_syntax();
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
                        let kind = if Some(source_module.id) == syntax
                            && declaration.name == "quote"
                        {
                            MacroKind::Quote
                        } else if Some(source_module.id) == syntax
                            && declaration.name == "parse_quote"
                        {
                            MacroKind::ParseQuote
                        } else if Some(source_module.id) == cinterop
                            && declaration.name == "c_string"
                        {
                            MacroKind::CString
                        } else if let Some(value) = &declaration.value {
                            MacroKind::User(value.clone())
                        } else {
                            MacroKind::User(Expression::Product(crate::ProductExpression::empty()))
                        };
                        let (arity, mut parameters, mut result) =
                            if matches!(kind, MacroKind::Quote | MacroKind::ParseQuote) {
                                let (parameters, result) = declaration
                                    .annotation
                                    .as_ref()
                                    .and_then(compiler_macro_signature)
                                    .unwrap_or((Vec::new(), MetaType::Syntax));
                                (parameters.len(), parameters, result)
                            } else {
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
                                (arity, parameters, result)
                            };
                        if declaration.modifier && declaration.annotation.is_none() {
                            if parameters.len() == 2 {
                                if parameters[0] == MetaType::SyntaxNode {
                                    parameters[0] = MetaType::Expr;
                                }
                                parameters[0] = MetaType::Delimited(
                                    DelimiterKind::Parenthesized,
                                    DelimitedMetaContents::Fixed(vec![parameters[0].clone()]),
                                );
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
                    Item::Binding(binding) => {
                        if binding.value.is_some() {
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
        // A companion (or ordinary `pub mod`) submodule is a macro-callable
        // namespace, e.g. `List.of(...)` for a macro `of` declared inside
        // `companion<T> List T { ... }`. Seeded here and carried through the
        // same re-export fixed point as `public_macros`/`public_helpers`
        // below, so `pub use Module.(Name)` re-exports the namespace the
        // same way it re-exports a value or type — mirroring how
        // `resolve.rs`'s `export_interface_item` treats `namespaces` as just
        // another kind of interface entry.
        let mut public_namespaces = HashMap::<(ModuleId, String), ModuleId>::new();
        for source_module in program.modules() {
            for item in &source_module.syntax.items {
                if let Item::Submodule(submodule) = item
                    && submodule.visibility == Visibility::Public
                    && let Some(child) = program.child_module(submodule.syntax.id)
                {
                    public_namespaces.insert((source_module.id, submodule.name.clone()), child);
                }
            }
        }

        loop {
            let previous_macros = public_macros.clone();
            let previous_helpers = public_helpers.clone();
            let previous_namespaces = public_namespaces.clone();
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
                    let names = match provisional_use_kind(program, use_) {
                        UseKind::Dotted => Vec::new(),
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
                            .chain(
                                previous_namespaces
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
                        if let Some(helper) = previous_helpers.get(&(imported, item.clone())) {
                            changed |= public_helpers
                                .insert((source_module.id, alias.clone()), helper.clone())
                                .is_none();
                        }
                        if let Some(namespace) = previous_namespaces.get(&(imported, item)) {
                            changed |= public_namespaces
                                .insert((source_module.id, alias), *namespace)
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
        let all_namespaces = program
            .modules()
            .iter()
            .flat_map(|module| {
                module.syntax.items.iter().filter_map(move |item| {
                    let Item::Submodule(submodule) = item else {
                        return None;
                    };
                    let child = program.child_module(submodule.syntax.id)?;
                    Some(((module.id, submodule.name.clone()), child))
                })
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
                for ((module, name), target) in &public_namespaces {
                    if *module == core {
                        scopes[source_module.id.0]
                            .namespaces
                            .entry(name.clone())
                            .or_insert_with(|| *target);
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
                let (macros, helpers, namespaces) = if use_.visibility == Visibility::Private
                    && macro_is_ancestor(program, imported, source_module.id)
                {
                    (&all_macros, &all_helpers, &all_namespaces)
                } else {
                    (&public_macros, &public_helpers, &public_namespaces)
                };
                match provisional_use_kind(program, use_) {
                    UseKind::Dotted => {}
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
                        for ((module, name), target) in namespaces {
                            if *module == imported {
                                scopes[source_module.id.0]
                                    .namespaces
                                    .entry(name.clone())
                                    .or_insert_with(|| *target);
                            }
                        }
                    }
                    UseKind::Selected(names) => {
                        for name in names {
                            Self::install_selected(
                                &mut scopes[source_module.id.0],
                                imported,
                                &name,
                                &name,
                                macros,
                                helpers,
                                namespaces,
                            );
                        }
                    }
                    UseKind::Renamed { item, alias } => Self::install_selected(
                        &mut scopes[source_module.id.0],
                        imported,
                        &item,
                        &alias,
                        macros,
                        helpers,
                        namespaces,
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
            imported_modules: program.imported_modules().clone(),
            use_kinds: program
                .modules()
                .iter()
                .flat_map(|module| {
                    module.syntax.items.iter().filter_map(|item| match item {
                        Item::UseDeclaration(declaration) => Some((
                            declaration.syntax.id,
                            provisional_use_kind(program, declaration),
                        )),
                        _ => None,
                    })
                })
                .collect(),
            child_modules: program.child_modules().clone(),
            parent_modules: program
                .modules()
                .iter()
                .map(|module| program.parent_module(module.id))
                .collect(),
            companion_modules: program
                .modules()
                .iter()
                .map(|module| module.companion)
                .collect(),
            diagnostics: Vec::new(),
            next_syntax_id,
            next_mark: 1,
            steps: 0,
            expansion_stack: Vec::new(),
            emitted_items: None,
            invocations: HashMap::new(),
            quote_context: None,
            helper_eval_depth: 0,
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
            helpers: self
                .scopes
                .iter()
                .flat_map(|scope| scope.helpers.values())
                .filter(|helper| binding_is_compile_time_helper(&helper.binding))
                .map(|helper| (helper.module, helper.binding.clone()))
                .collect(),
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
        public_namespaces: &HashMap<(ModuleId, String), ModuleId>,
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
        if let Some(target) = public_namespaces.get(&(imported, item.to_owned())) {
            scope
                .namespaces
                .entry(local.to_owned())
                .or_insert_with(|| *target);
        }
    }

    fn validate_definitions(&mut self) {
        let mut groups = HashMap::<(ModuleId, String, bool), Vec<MacroDefinition>>::new();
        for definition in self.definitions.values() {
            if !definition.declaration.type_parameters.is_empty()
                && !matches!(definition.kind, MacroKind::ParseQuote)
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "generic user-defined macros are not supported",
                ));
            }
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
                    .is_some_and(invalid_separated_parameter)
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "`Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`",
                ));
            }
            if let Some(annotation) = &definition.declaration.annotation
                && !matches!(definition.kind, MacroKind::ParseQuote)
                && !valid_macro_annotation(annotation)
            {
                self.diagnostics.push(Diagnostic::new(
                    annotation.syntax().span.clone(),
                    if type_contains_named(annotation, "Separated")
                    {
                        "`Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`"
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
            validate_top_level_sequence(definition, &mut self.diagnostics);
            if definition.parameters.iter().any(invalid_raw_syntax_shape) {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "opaque `Syntax` may be a top-level argument or the entire contents of a delimiter, but cannot be partitioned by `Sequence`, `Separated`, `Optional`, or product shapes",
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
                        && body_parameter == Some(MetaType::SyntaxNode);
                    let effective_declared =
                        if definition.key.modifier && index + 1 != parameters.len() {
                            modifier_argument_meta_type(declared).unwrap_or(declared)
                        } else {
                            declared
                        };
                    if !implicit_modifier_item
                        && body_parameter
                            .is_some_and(|body_parameter| body_parameter != *effective_declared)
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
                if matches!(
                    definition.key.name.as_str(),
                    "recursive_constructor" | "doc"
                ) {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "modifier name `@{}` is reserved by the compiler",
                            definition.key.name
                        ),
                    ));
                }
                let valid_parameters = match definition.parameters.as_slice() {
                    [MetaType::Item] => true,
                    [leading, MetaType::Item] => {
                        modifier_argument_meta_type(leading).is_some_and(|argument| {
                            matches!(
                                argument,
                                MetaType::Expr
                                    | MetaType::Ident(_)
                                    | MetaType::CallExpr
                                    | MetaType::UnstructuredExpr
                                    | MetaType::Type
                                    | MetaType::Pattern
                            )
                        })
                    }
                    _ => false,
                };
                let valid_result = matches!(definition.result, MetaType::Item | MetaType::Syntax)
                    || matches!(&definition.result, MetaType::Sequence(inner) if **inner == MetaType::Item);
                if !valid_parameters || !valid_result {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "modifier macro `@{}` must have signature `Item -> Item`, `Item -> Sequence Item`, or `Item -> Syntax`, optionally with a leading `Parenthesized (Expr | Type | Pattern) ->` argument",
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
                    if quote_result_type(&definition.result)
                        && quote_at_tail(body, definition.arity) =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        body.syntax().span.clone(),
                        format!(
                            "macro `{}` declares result `{}`, but its body ends in `quote`, which always returns opaque `Syntax`; use `parse_quote` instead",
                            definition.key.name,
                            format_meta_type(&definition.result)
                        ),
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
            match self.apply_modifier_chain(module, modified, depth) {
                Some(ModifierChainResult::Item(expanded)) => {
                    *item = expanded;
                    self.expand_item(module, item, depth + 1);
                }
                Some(ModifierChainResult::Items(items)) => {
                    self.emitted_items = Some(items);
                }
                None => {}
            }
            return;
        }
        if let Item::Expression(expression) = item {
            let expression = expression.clone();
            if self.expand_top_level_macro(module, item, expression, depth) {
                return;
            }
        }
        match item {
            Item::Binding(binding) if binding_is_compile_time_helper(binding) => {}
            item @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.expand_block_item_contents(module, item, depth),
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
                    Some(Item::Expression(expression))
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
    ) -> Option<ModifierChainResult> {
        let mut current = *modified.item;
        let mut trailing: Vec<Item> = Vec::new();
        if let Item::Modified(nested) = current {
            let result = self.apply_modifier_chain(module, nested, depth + 1)?;
            let (first, extra) = split_modifier_chain_result(result);
            trailing.extend(extra);
            match first {
                Some(item) => current = item,
                None if modified.modifiers.is_empty() => {
                    return Some(ModifierChainResult::Items(trailing));
                }
                None => {
                    self.diagnostics.push(Diagnostic::new(
                        modified.syntax.span,
                        "modifier macro chain produced no item, so the remaining modifiers have nothing to apply to",
                    ));
                    return None;
                }
            }
        }
        if let Item::VisibilityMacroInvocation(invocation) = current {
            current = self.expand_visibility_macro_invocation(module, invocation, depth + 1)?;
        }
        let docs_only = modified
            .modifiers
            .iter()
            .all(|modifier| modifier.namespace.is_none() && modifier.name == "doc");
        if !modifier_target_supported(&current) && !docs_only {
            self.diagnostics.push(Diagnostic::new(
                modified.syntax.span,
                "modifier macros may only be applied to `let`, `def`, `type`, `extern`, `trait`, or `impl` items",
            ));
            return None;
        }

        let modifiers = modified.modifiers.into_iter().rev().collect::<Vec<_>>();
        let last_index = modifiers.len().saturating_sub(1);
        for (index, invocation) in modifiers.into_iter().enumerate() {
            let is_last = index == last_index;
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
            if invocation.namespace.is_none() && invocation.name == "doc" {
                let doc = if let Some(doc) = invocation.doc.clone() {
                    doc
                } else {
                    let Some(argument) = invocation.argument.as_ref() else {
                        self.diagnostics.push(Diagnostic::new(
                            invocation.syntax.span,
                            "`@doc` requires a parenthesized string literal",
                        ));
                        return None;
                    };
                    let Some(Expression::String(literal)) = argument.expression.as_ref() else {
                        self.diagnostics.push(Diagnostic::new(
                            invocation.syntax.span,
                            "`@doc` requires a string literal argument",
                        ));
                        return None;
                    };
                    match crate::string_literal::decode(&literal.literal) {
                        Ok(doc) => doc,
                        Err(message) => {
                            self.diagnostics
                                .push(Diagnostic::new(invocation.syntax.span, message));
                            return None;
                        }
                    }
                };
                if !attach_doc(&mut current, doc) {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span,
                        "`@doc` may only modify a named declaration",
                    ));
                    return None;
                }
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
                module,
                &definition,
                argument,
                current,
                invocation.syntax.span.clone(),
            );
            let Some(items) = result else {
                self.expansion_stack.pop();
                if self.diagnostics.len() > diagnostic_start {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!("while expanding modifier macro `@{}`", key.name),
                    ));
                }
                return None;
            };
            // The first item produced by this invocation continues the chain (feeding the
            // next modifier, or becoming part of the final result); any further items are
            // deferred as `trailing`, to be spliced in after the chain's ultimate result.
            let (first, mut extra) = split_first_item(items);
            let first = match first {
                Some(item @ Item::Modified(_)) => {
                    let Item::Modified(nested) = item else {
                        unreachable!()
                    };
                    let nested_result = self.apply_modifier_chain(module, nested, depth + 1);
                    self.expansion_stack.pop();
                    let (nested_first, nested_extra) = split_modifier_chain_result(nested_result?);
                    extra.extend(nested_extra);
                    nested_first
                }
                other => {
                    self.expansion_stack.pop();
                    other
                }
            };
            let Some(first) = first else {
                if is_last {
                    extra.extend(trailing);
                    return Some(ModifierChainResult::Items(extra));
                }
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span.clone(),
                    format!(
                        "modifier macro `@{}` produced no items, so the remaining modifiers in the chain have nothing to apply to",
                        key.name
                    ),
                ));
                return None;
            };
            if !is_last {
                if !modifier_target_supported(&first) {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!(
                            "modifier macro `@{}` produced an unsupported item kind",
                            key.name
                        ),
                    ));
                    return None;
                }
                trailing.extend(extra);
                current = first;
            } else if extra.is_empty() && trailing.is_empty() {
                if !modifier_target_supported(&first) {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!(
                            "modifier macro `@{}` produced an unsupported item kind",
                            key.name
                        ),
                    ));
                    return None;
                }
                return Some(ModifierChainResult::Item(first));
            } else {
                let mut batch = vec![first];
                batch.extend(extra);
                batch.extend(trailing);
                return Some(ModifierChainResult::Items(batch));
            }
        }
        Some(ModifierChainResult::Item(current))
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
                    (Some(argument), [leading, MetaType::Item]) => {
                        modifier_argument_meta_type(leading)
                            .is_some_and(|expected| modifier_argument_matches(expected, argument))
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
            (Some(argument), [leading, MetaType::Item]) => {
                let expected = modifier_argument_meta_type(leading)
                    .expect("selected modifier signature must match its invocation");
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
        module: ModuleId,
        definition: &MacroDefinition,
        argument: Option<SyntaxValue>,
        item: Item,
        call_span: Span,
    ) -> Option<Vec<Item>> {
        let MacroKind::User(body) = &definition.kind else {
            unreachable!("compiler-provided macros cannot be modifiers")
        };
        let previous_context = if quote_result_type(&definition.result) {
            self.quote_context.replace(definition.result.clone())
        } else {
            self.quote_context.take()
        };
        let evaluated = (|| {
            let mut value =
                self.eval_expression(definition.key.module, body, &mut Environment::new())?;
            if let Some(argument) = argument {
                value = self.apply_value(value, Value::Syntax(argument), call_span.clone())?;
            }
            self.apply_value(
                value,
                Value::Syntax(SyntaxValue::Item(Box::new(item))),
                call_span,
            )
        })();
        self.quote_context = previous_context;
        let value = evaluated?;
        match value {
            Value::Syntax(SyntaxValue::Item(item)) => Some(vec![*item]),
            Value::Syntax(SyntaxValue::Items(items)) => Some(items),
            Value::Sequence(values) if matches!(&definition.result, MetaType::Sequence(element) if **element == MetaType::Item) => {
                values
                    .into_iter()
                    .map(|value| match value {
                        Value::Syntax(SyntaxValue::Item(item)) => Some(*item),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()
            }
            Value::Syntax(SyntaxValue::Raw(raw)) => {
                let mut items = match crate::parser::parse_item_list_fragment(
                    &raw,
                    &mut self.next_syntax_id,
                ) {
                    Ok(items) => items,
                    Err(error) => {
                        self.diagnostics.push(Diagnostic::new(
                            definition.declaration.syntax.span.clone(),
                            format!("modifier macro `@{}` produced a syntax fragment that is not valid in item position: {}", definition.key.name, error.message),
                        ));
                        return None;
                    }
                };
                let mark = self.next_mark;
                self.next_mark += 1;
                for generated in &mut items {
                    alpha_rename_item(generated, mark);
                    freshen_item(self, generated, module, mark);
                }
                Some(items)
            }
            Value::Syntax(syntax) => {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    format!(
                        "modifier macro `@{}` must return `Item`, `Sequence Item`, or `Syntax`, but returned {} syntax",
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
                        "modifier macro `@{}` did not return `Item`, `Sequence Item`, or `Syntax`",
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
            MetaType::Syntax | MetaType::SyntaxNode | MetaType::Item | MetaType::Sequence(_)
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
            SyntaxValue::Raw(raw) => {
                let parsed =
                    crate::parser::parse_item_list_fragment(&raw, &mut self.next_syntax_id);
                let Ok(mut items) = parsed else {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!(
                            "macro `{}` produced a syntax fragment that is not valid in item position",
                            key.name
                        ),
                    ));
                    self.expansion_stack.pop();
                    return true;
                };
                let mark = self.next_mark;
                self.next_mark += 1;
                for generated in &mut items {
                    alpha_rename_item(generated, mark);
                    freshen_item(self, generated, module, mark);
                }
                if items.len() == 1 {
                    let mut generated = items.remove(0);
                    if !arguments[consumed_count..].is_empty() {
                        let Item::Expression(result) = &generated else {
                            self.diagnostics.push(Diagnostic::new(
                                expression.syntax().span.clone(),
                                format!(
                                    "item-producing macro `{}` cannot have excess arguments",
                                    key.name
                                ),
                            ));
                            self.expansion_stack.pop();
                            return true;
                        };
                        let mut result = result.clone();
                        for argument in &arguments[consumed_count..] {
                            let mut syntax = result.syntax().clone();
                            syntax.id = self.fresh_id();
                            result = Expression::Call(crate::CallExpression {
                                syntax,
                                callee: Box::new(result),
                                argument: Box::new((*argument).clone()),
                            });
                        }
                        generated = Item::Expression(result);
                    }
                    *item = generated;
                    self.expand_item(module, item, depth + 1);
                } else if !arguments[consumed_count..].is_empty() {
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
                    *item = Item::Expression(result);
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

    fn expand_block_item_contents(&mut self, module: ModuleId, item: &mut Item, depth: usize) {
        match item {
            Item::Binding(binding) => {
                if let Some(value) = binding.value.take() {
                    binding.value = Some(self.expand_expression(module, value, depth));
                }
                if binding.kind == BindingKind::Const
                    && let Some(value) = &binding.value
                {
                    let span = value.syntax().span.clone();
                    let folded = self
                        .eval_expression(module, value, &mut Environment::new())
                        .and_then(|result| self.value_to_expression(result, span));
                    if let Some(folded) = folded {
                        binding.value = Some(folded);
                    }
                }
            }
            Item::PatternBinding(binding) => {
                binding.value = self.expand_expression(module, binding.value.clone(), depth);
            }
            Item::Assignment(assignment) => {
                assignment.target =
                    self.expand_expression(module, assignment.target.clone(), depth);
                assignment.value = self.expand_expression(module, assignment.value.clone(), depth);
            }
            Item::Return(return_) => {
                return_.value = self.expand_expression(module, return_.value.clone(), depth);
            }
            Item::Break(break_) => {
                if let Some(value) = break_.value.take() {
                    break_.value = Some(self.expand_expression(module, value, depth));
                }
            }
            Item::Continue(_) => {}
            Item::Expression(expression) => {
                *expression = self.expand_expression(module, expression.clone(), depth);
            }
            // A block-scoped `mod` is its own flat `SourceModule`, expanded
            // independently when the top-level driver reaches it — mirrors
            // the `Item::Submodule(_) => {}` catch-all in `expand_block_item`.
            Item::Submodule(_) => {}
            // Types carry no runtime macro calls to expand — mirrors the
            // `Item::TypeDeclaration(_) => {}` catch-all in `expand_block_item`.
            Item::TypeDeclaration(_) => {}
            // A use declaration's path carries no runtime macro calls either
            // — mirrors `Item::UseDeclaration(_) => {}` in `expand_block_item`.
            Item::UseDeclaration(_) => {}
            _ => unreachable!("unsupported item reached item expansion"),
        }
    }

    fn expand_block(&mut self, module: ModuleId, block: &mut BlockExpression, depth: usize) {
        let outer = self.scopes[module.0].clone();
        for item in &block.items {
            match item {
                Item::Submodule(submodule) => {
                    if let Some(child) = self.child_modules.get(&submodule.syntax.id).copied() {
                        self.scopes[module.0]
                            .namespaces
                            .insert(submodule.name.clone(), child);
                    }
                }
                Item::UseDeclaration(declaration) => {
                    self.install_block_import(module, declaration);
                }
                _ => {}
            }
        }
        let mut expanded = Vec::new();
        for item in std::mem::take(&mut block.items) {
            expanded.extend(self.expand_block_item(module, item, depth));
        }
        block.items = expanded;
        self.scopes[module.0] = outer;
    }

    fn expand_block_item(&mut self, module: ModuleId, item: Item, depth: usize) -> Vec<Item> {
        let generated = if let Item::Modified(modified) = item {
            match self.apply_modifier_chain(module, modified, depth) {
                Some(ModifierChainResult::Item(item)) => vec![item],
                Some(ModifierChainResult::Items(items)) => items,
                None => return Vec::new(),
            }
        } else {
            vec![item]
        };

        let mut expanded = Vec::new();
        for generated_item in generated {
            if matches!(generated_item, Item::Modified(_)) {
                expanded.extend(self.expand_block_item(module, generated_item, depth + 1));
                continue;
            }
            if !block_item_supported(&generated_item) {
                self.diagnostics.push(Diagnostic::new(
                    item_syntax(&generated_item).span.clone(),
                    "item is not supported in a block expression",
                ));
                continue;
            }
            let mut generated_item = generated_item;
            if let Item::Submodule(submodule) = &mut generated_item
                && submodule.syntax.definition_module().is_some()
            {
                let items = std::mem::take(&mut submodule.module.items);
                submodule.module.items = self.expand_generated_items(module, items, depth + 1);
            }
            self.expand_block_item_contents(module, &mut generated_item, depth + 1);
            expanded.push(generated_item);
        }
        expanded
    }

    fn install_block_import(&mut self, module: ModuleId, use_: &UseDeclaration) {
        let Some(imported) = self.imported_modules.get(&use_.syntax.id).copied() else {
            return;
        };
        let imported_scope = self.scopes[imported.0].clone();
        let mut cursor = module;
        let mut include_private = false;
        while let Some(parent) = self.parent_modules[cursor.0] {
            if parent == imported {
                include_private = true;
                break;
            }
            cursor = parent;
        }
        let visible_keys = |keys: &[MacroKey]| {
            keys.iter()
                .filter(|key| {
                    include_private
                        || self.definitions[key].declaration.visibility == Visibility::Public
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let install = |item: &str, alias: &str, scope: &mut ModuleScope| {
            if let Some(keys) = imported_scope.macros.get(item) {
                let keys = visible_keys(keys);
                if !keys.is_empty() {
                    scope.macros.entry(alias.to_owned()).or_insert(keys);
                }
            }
            if let Some(keys) = imported_scope.modifiers.get(item) {
                let keys = visible_keys(keys);
                if !keys.is_empty() {
                    scope.modifiers.entry(alias.to_owned()).or_insert(keys);
                }
            }
            if let Some(helper) = imported_scope.helpers.get(item)
                && (include_private || helper.binding.visibility == Visibility::Public)
            {
                scope
                    .helpers
                    .entry(alias.to_owned())
                    .or_insert_with(|| helper.clone());
            }
        };
        let use_kind = self.use_kind(use_);
        let scope = &mut self.scopes[module.0];
        match use_kind {
            UseKind::Dotted => {}
            UseKind::Namespace => {
                if let Some(name) = use_.path.last() {
                    scope.namespaces.insert(name.clone(), imported);
                }
            }
            UseKind::Glob => {
                for name in imported_scope
                    .macros
                    .keys()
                    .chain(imported_scope.modifiers.keys())
                {
                    install(&name, &name, scope);
                }
                for name in imported_scope.helpers.keys() {
                    install(name, name, scope);
                }
            }
            UseKind::Selected(names) => {
                for name in names {
                    install(&name, &name, scope);
                }
            }
            UseKind::Renamed { item, alias } => install(&item, &alias, scope),
        }
    }

    fn use_kind(&self, declaration: &UseDeclaration) -> UseKind {
        self.use_kinds
            .get(&declaration.syntax.id)
            .cloned()
            .unwrap_or_else(|| declaration.kind.clone())
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
            if !matches!(definition.result, MetaType::Syntax | MetaType::SyntaxNode)
                && !definition.result.is_expression()
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
            let result = if let SyntaxValue::Raw(raw) = result {
                match crate::parser::parse_expression_fragment(&raw, &mut self.next_syntax_id) {
                    Ok(mut parsed) => {
                        let mark = self.next_mark;
                        self.next_mark += 1;
                        alpha_rename_expression(&mut parsed, mark, &mut Vec::new());
                        self.freshen_expression(&mut parsed, module, mark);
                        SyntaxValue::from_expression(parsed)
                    }
                    Err(error) => {
                        self.diagnostics.push(Diagnostic::new(
                            expression.syntax().span.clone(),
                            format!(
                                "macro `{}` produced a syntax fragment that is not valid in expression position: {}",
                                key.name, error.message
                            ),
                        ));
                        self.expansion_stack.pop();
                        return expression;
                    }
                }
            } else {
                result
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
                self.expand_block(module, &mut loop_.body, depth);
                Expression::Loop(loop_)
            }
            Expression::With(mut with) => {
                with.value = Box::new(self.expand_expression(module, *with.value, depth));
                self.expand_block(module, &mut with.body, depth);
                Expression::With(with)
            }
            Expression::Block(mut block) => {
                self.expand_block(module, &mut block, depth);
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
            Expression::Quote(quote) => {
                self.diagnostics.push(Diagnostic::new(
                    quote.syntax.span.clone(),
                    format!(
                        "`{}` is only available in a macro body or compile-time helper",
                        quote.kind.name()
                    ),
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
                let mut cursor = context;
                loop {
                    if self.scopes[cursor.0].helpers.contains_key(&name.name) {
                        return None;
                    }
                    if let Some(keys) = self.scopes[cursor.0].macros.get(&name.name) {
                        return Some(keys.clone());
                    }
                    if !self.companion_modules[cursor.0] {
                        return None;
                    }
                    cursor = self.parent_modules[cursor.0]?;
                }
            }
            Expression::Access(access) => {
                let (namespace, item, definition_module) = qualified_macro_access_path(access)?;
                let context = definition_module.map(ModuleId).unwrap_or(module);
                let mut cursor = context;
                let target = loop {
                    if let Some(target) = self.scopes[cursor.0].namespaces.get(&namespace) {
                        break *target;
                    }
                    if !self.companion_modules[cursor.0] {
                        return None;
                    }
                    cursor = self.parent_modules[cursor.0]?;
                };
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
                    |(matched_arguments, consumed, effective_parameters)| SelectedMacro {
                        definition: definition.clone(),
                        arguments: matched_arguments,
                        consumed,
                        effective_parameters,
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
                && !definition
                    .parameters
                    .iter()
                    .any(|parameter| matches!(parameter, MetaType::Sequence(_)))
                && arguments.len() >= definition.arity
            {
                self.validate_macro_arguments(definition, &arguments[..definition.arity]);
                return None;
            }
            let mut arities = definitions
                .iter()
                .filter(|definition| {
                    !has_visibility_parameters
                        && !definition
                            .parameters
                            .iter()
                            .any(|parameter| matches!(parameter, MetaType::Sequence(_)))
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
                        && effective_signature_more_specific(
                            &other.effective_parameters,
                            &candidate.effective_parameters,
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
            MacroKind::ParseQuote => {
                self.diagnostics.push(Diagnostic::new(
                    call_span,
                    "`parse_quote` may only be evaluated inside a macro body",
                ));
                None
            }
            MacroKind::User(body) => {
                let previous_context = if quote_result_type(&definition.result) {
                    self.quote_context.replace(definition.result.clone())
                } else {
                    self.quote_context.take()
                };
                let evaluated = (|| {
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
                            MacroArgument::Sequence(arguments) => {
                                let MetaType::Sequence(element) = expected else {
                                    return None;
                                };
                                let values = arguments
                                    .iter()
                                    .map(|argument| {
                                        self.meta_argument_value(element, argument)
                                            .map(Value::Syntax)
                                    })
                                    .collect::<Option<Vec<_>>>()?;
                                value = self.apply_value(
                                    value,
                                    Value::Sequence(values),
                                    call_span.clone(),
                                )?;
                                continue;
                            }
                        };
                        value =
                            self.apply_value(value, Value::Syntax(argument), call_span.clone())?;
                    }
                    Some(value)
                })();
                self.quote_context = previous_context;
                match evaluated? {
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
                MetaType::Syntax
                    | MetaType::SyntaxNode
                    | MetaType::Visibility
                    | MetaType::MacroCallVisibility
            )
            .then(|| SyntaxValue::Visibility(visibility.clone()));
        }
        match expected {
            MetaType::Type => {
                parse_type_argument(argument, &mut self.next_syntax_id).map(SyntaxValue::Type)
            }
            MetaType::Pattern => {
                parse_pattern_argument(argument, &mut self.next_syntax_id).map(SyntaxValue::Pattern)
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
                    MetaType::StringExpr => "a string-literal expression".to_owned(),
                    MetaType::UnstructuredExpr => "an unstructured expression".to_owned(),
                    MetaType::Type => "a type".to_owned(),
                    MetaType::Pattern => "a pattern".to_owned(),
                    MetaType::BindingPattern => "a binding pattern".to_owned(),
                    MetaType::NominalPattern => "a nominal pattern".to_owned(),
                    MetaType::Item => "an item".to_owned(),
                    MetaType::TypeDeclarationItem => "a type declaration item".to_owned(),
                    MetaType::UnstructuredItem => "an unstructured item".to_owned(),
                    MetaType::Visibility => "visibility syntax".to_owned(),
                    MetaType::MacroCallVisibility => "macro-call visibility".to_owned(),
                    MetaType::Comma => "comma syntax".to_owned(),
                    MetaType::Equals => "equals syntax".to_owned(),
                    MetaType::FatArrow => "fat-arrow syntax".to_owned(),
                    MetaType::Product(_) | MetaType::Optional(_) | MetaType::Sequence(_) => {
                        format!("`{}` syntax", format_meta_type(expected))
                    }
                    MetaType::Syntax | MetaType::SyntaxNode | MetaType::Expr => {
                        "an expression".to_owned()
                    }
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
            Expression::StringTemplate(_) => {
                self.diagnostics.push(Diagnostic::new(
                    expression.syntax().span.clone(),
                    "string templates are not available during compile-time macro evaluation",
                ));
                None
            }
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
                quote_result: self.quote_context.clone(),
            }),
            Expression::Satisfies(satisfies) => {
                if let Some(expected) = meta_type(&satisfies.ty)
                    && let Expression::Quote(quote) = satisfies.value.as_ref()
                    && quote.kind == crate::QuoteKind::ParseQuote
                {
                    if !quote_result_type(&expected) {
                        self.diagnostics.push(Diagnostic::new(
                            satisfies.ty.syntax().span.clone(),
                            format!(
                                "{} is not a supported `parse_quote` context",
                                format_meta_type(&expected)
                            ),
                        ));
                        return None;
                    }
                    return self.instantiate_contextual_quote(
                        module,
                        quote,
                        environment,
                        &expected,
                    );
                }
                self.eval_expression(module, &satisfies.value, environment)
            }
            Expression::Quote(quote) => {
                let expected = match quote.kind {
                    crate::QuoteKind::Quote => MetaType::Syntax,
                    crate::QuoteKind::ParseQuote => match self.quote_context.clone() {
                        Some(expected) => expected,
                        None => {
                            self.diagnostics.push(Diagnostic::new(
                                quote.syntax.span.clone(),
                                "`parse_quote` requires a contextual syntax type; annotate the binding, use `satisfies`, or declare the macro's result type",
                            ));
                            return None;
                        }
                    },
                };
                self.instantiate_contextual_quote(module, quote, environment, &expected)
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
                let mut cursor = module;
                loop {
                    if let Some(helper) = self.scopes[cursor.0].helpers.get(&name.name).cloned() {
                        if matches!(helper.binding.value, Some(Expression::Function(_))) {
                            return Some(Value::Helper(helper.module, helper.binding));
                        }
                        let value = helper.binding.value?;
                        return self.eval_helper_value(
                            helper.module,
                            &value,
                            name.syntax.span.clone(),
                        );
                    }
                    if !self.companion_modules[cursor.0] {
                        break;
                    }
                    let Some(parent) = self.parent_modules[cursor.0] else {
                        break;
                    };
                    cursor = parent;
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
            Expression::Float(float) => float.literal.parse::<f64>().ok().map(Value::Float),
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
                let has_named_spread = product.elements.iter().any(|element| element.named_spread);
                let mut values: Vec<(Option<String>, Value)> = Vec::new();
                for element in &product.elements {
                    let sequence_element = match self.quote_context.clone() {
                        Some(MetaType::Sequence(expected)) => Some(*expected),
                        _ => None,
                    };
                    let value = if let Expression::Quote(quote) = &element.value
                        && quote.kind == crate::QuoteKind::ParseQuote
                        && let Some(expected) = sequence_element
                    {
                        self.instantiate_contextual_quote(module, quote, environment, &expected)?
                    } else {
                        self.eval_expression(module, &element.value, environment)?
                    };
                    if element.spread {
                        let Value::Product(elements) = value else {
                            self.diagnostics.push(Diagnostic::new(
                                element.syntax.span.clone(),
                                "compile-time product spread requires a product value",
                            ));
                            return None;
                        };
                        if element.named_spread {
                            for (name, value) in elements {
                                let Some(name) = name else {
                                    self.diagnostics.push(Diagnostic::new(
                                        element.syntax.span.clone(),
                                        "a named spread operand must have every element named",
                                    ));
                                    return None;
                                };
                                set_named_product_field(&mut values, name, value);
                            }
                        } else {
                            values.extend(elements);
                        }
                    } else if has_named_spread {
                        let Some(name) = element.name.clone() else {
                            self.diagnostics.push(Diagnostic::new(
                                element.syntax.span.clone(),
                                "every element must be named when the product contains a named spread",
                            ));
                            return None;
                        };
                        set_named_product_field(&mut values, name, value);
                    } else {
                        values.push((element.name.clone(), value));
                    }
                }
                Some(Value::Product(values))
            }
            Expression::Call(call) => {
                let (head, arguments) = flatten_call(expression);
                if let Some(result) =
                    self.eval_builtin_operator_call(module, head, &arguments, environment)
                {
                    return result;
                }
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
                    if let Some(SyntaxValue::Raw(raw)) = result.take() {
                        result = match crate::parser::parse_expression_fragment(
                            &raw,
                            &mut self.next_syntax_id,
                        ) {
                            Ok(mut parsed) => {
                                let mark = self.next_mark;
                                self.next_mark += 1;
                                alpha_rename_expression(&mut parsed, mark, &mut Vec::new());
                                self.freshen_expression(&mut parsed, module, mark);
                                Some(SyntaxValue::from_expression(parsed))
                            }
                            Err(error) => {
                                self.diagnostics.push(Diagnostic::new(
                                    expression.syntax().span.clone(),
                                    format!(
                                        "macro `{}` produced a syntax fragment that is not valid in expression position: {}",
                                        key.name, error.message
                                    ),
                                ));
                                None
                            }
                        };
                    }
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
                            | "StringExpr"
                            | "BindingPattern"
                            | "NominalPattern"
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
                let mut value = self.eval_expression(module, &access.value, environment)?;
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
                        Accessor::Method(_) => None,
                        Accessor::Representation => {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                "call syntax has no nominal representation",
                            ));
                            None
                        }
                    };
                }
                if let Value::Syntax(SyntaxValue::Item(item)) = &value {
                    if let Item::TypeDeclaration(declaration) = item.as_ref() {
                        value = type_declaration_item_value(declaration);
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            "unstructured item syntax has no inspectable fields",
                        ));
                        return None;
                    }
                }
                if matches!(access.accessor, Accessor::Representation) {
                    if let Value::Nominal(_, representation) = value {
                        return Some(*representation);
                    }
                    self.diagnostics.push(Diagnostic::new(
                        access.syntax.span.clone(),
                        "compile-time representation access requires a nominal value",
                    ));
                    return None;
                }
                if let Value::Nominal(_, representation) = value {
                    value = *representation;
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
                    Accessor::Method(_) => None,
                    Accessor::Representation => unreachable!("handled above"),
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
            Expression::Logical(logical) => {
                let left = self.eval_expression(module, &logical.left, environment)?;
                let Value::Nominal(name, _) = &left else {
                    self.diagnostics.push(Diagnostic::new(
                        logical.left.syntax().span.clone(),
                        "compile-time `&&`/`||` requires a `Bool` value",
                    ));
                    return None;
                };
                let short_circuits = match (logical.operator, name.as_str()) {
                    (LogicalOperator::And, "False") | (LogicalOperator::Or, "True") => true,
                    (LogicalOperator::And, "True") | (LogicalOperator::Or, "False") => false,
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            logical.left.syntax().span.clone(),
                            "compile-time `&&`/`||` requires a `Bool` value",
                        ));
                        return None;
                    }
                };
                if short_circuits {
                    Some(left)
                } else {
                    self.eval_expression(module, &logical.right, environment)
                }
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
                for item in &block.items {
                    match item {
                        Item::Binding(binding) => {
                            let Some(value) = &binding.value else {
                                self.diagnostics.push(Diagnostic::new(
                                    binding.syntax.span.clone(),
                                    "compile-time declarations require a value",
                                ));
                                return None;
                            };
                            let value = if let Some(expected) =
                                binding.annotation.as_ref().and_then(meta_type)
                                && let Expression::Quote(quote) = value
                                && quote.kind == crate::QuoteKind::ParseQuote
                            {
                                if !quote_result_type(&expected) {
                                    self.diagnostics.push(Diagnostic::new(
                                        binding.syntax.span.clone(),
                                        format!(
                                            "{} is not a supported `parse_quote` context",
                                            format_meta_type(&expected)
                                        ),
                                    ));
                                    return None;
                                }
                                self.instantiate_contextual_quote(module, quote, &local, &expected)?
                            } else {
                                self.eval_expression(module, value, &mut local)?
                            };
                            local.insert(
                                binding.name.clone(),
                                EnvironmentBinding::new(value, binding.mutable),
                            );
                        }
                        Item::PatternBinding(binding) => {
                            let value = self.eval_expression(module, &binding.value, &mut local)?;
                            if !match_pattern(&binding.pattern, &value, &mut local) {
                                return None;
                            }
                        }
                        Item::Assignment(assignment) => {
                            let value = if let Expression::Quote(quote) = &assignment.value
                                && quote.kind == crate::QuoteKind::ParseQuote
                                && matches!(
                                    &assignment.target,
                                    Expression::Access(access)
                                        if matches!(access.accessor, Accessor::Name(ref name) if name == "callee" || name == "argument")
                                ) {
                                self.instantiate_contextual_quote(
                                    module,
                                    quote,
                                    &local,
                                    &MetaType::Expr,
                                )?
                            } else {
                                self.eval_expression(module, &assignment.value, &mut local)?
                            };
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
                        Item::Expression(value) => {
                            result = self.eval_expression(module, value, &mut local)?
                        }
                        Item::Return(return_) => {
                            return self.eval_expression(module, &return_.value, &mut local);
                        }
                        Item::Break(break_) => {
                            self.diagnostics.push(Diagnostic::new(
                                break_.syntax.span.clone(),
                                "`break` is not supported during compile-time evaluation",
                            ));
                            return None;
                        }
                        Item::Continue(continue_) => {
                            self.diagnostics.push(Diagnostic::new(
                                continue_.syntax.span.clone(),
                                "`continue` is not supported during compile-time evaluation",
                            ));
                            return None;
                        }
                        Item::Submodule(submodule) => {
                            self.diagnostics.push(Diagnostic::new(
                                submodule.syntax.span.clone(),
                                "`mod` is not supported during compile-time evaluation",
                            ));
                            return None;
                        }
                        Item::TypeDeclaration(declaration) => {
                            self.diagnostics.push(Diagnostic::new(
                                declaration.syntax.span.clone(),
                                "type declarations are not supported during compile-time evaluation",
                            ));
                            return None;
                        }
                        Item::UseDeclaration(declaration) => {
                            self.diagnostics.push(Diagnostic::new(
                                declaration.syntax.span.clone(),
                                "`use` is not supported during compile-time evaluation",
                            ));
                            return None;
                        }
                        _ => return None,
                    }
                }
                Some(result)
            }
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
            "StringExpr" => {
                let Value::String(value) = argument else {
                    self.diagnostics
                        .push(Diagnostic::new(span, "`StringExpr` requires a string"));
                    return None;
                };
                let syntax = self.generated_syntax(module, span);
                Some(Value::Syntax(SyntaxValue::Unstructured(
                    Expression::String(crate::StringExpression {
                        syntax,
                        literal: crate::string_literal::encode(&value),
                    }),
                )))
            }
            "BindingPattern" => {
                let Value::Syntax(SyntaxValue::Ident(identifier)) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`BindingPattern` requires an identifier",
                    ));
                    return None;
                };
                Some(Value::Syntax(SyntaxValue::Pattern(Pattern::Binding(
                    crate::BindingPattern {
                        syntax: identifier.syntax,
                        mutable: false,
                        name: identifier.name.clone(),
                        resolution_name: Some(identifier.name),
                        ty: Type::Inferred(crate::InferredType::new()),
                    },
                ))))
            }
            "NominalPattern" => {
                let Value::Product(fields) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`NominalPattern` requires `(name: Ident String, argument: Pattern)`",
                    ));
                    return None;
                };
                let [
                    (_, Value::Syntax(SyntaxValue::Ident(name))),
                    (_, Value::Syntax(SyntaxValue::Pattern(argument))),
                ] = fields.as_slice()
                else {
                    self.diagnostics
                        .push(Diagnostic::new(span, "invalid `NominalPattern` fields"));
                    return None;
                };
                Some(Value::Syntax(SyntaxValue::Pattern(Pattern::Nominal(
                    crate::NominalPattern {
                        syntax: self.generated_syntax(module, span),
                        namespace: None,
                        name: name.name.clone(),
                        argument: Box::new(argument.clone()),
                    },
                ))))
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
                let contents =
                    match argument {
                        Value::Syntax(SyntaxValue::Raw(syntax)) => DelimitedValueContents::Fixed(
                            vec![(None, Value::Syntax(SyntaxValue::Raw(syntax)))],
                        ),
                        Value::Product(mut values)
                            if values.len() == 1 && matches!(values[0].1, Value::Sequence(_)) =>
                        {
                            let (_, Value::Sequence(values)) = values.remove(0) else {
                                unreachable!("guarded sequence contents")
                            };
                            DelimitedValueContents::Sequence(values)
                        }
                        Value::Product(mut values)
                            if values.len() == 1
                                && matches!(values[0].1, Value::Separated { .. }) =>
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
        if fields.is_empty() {
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
        } else if !binding.mutable {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "cannot write through immutable compile-time binding `{}`",
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

    /// Evaluates a named binding's raw initializer from scratch — the
    /// operation `Expression::Name` and `Value::Helper` application both
    /// perform to resolve a reference to a `def`/`const`/`let`. Bounded by
    /// `helper_eval_depth` so a self-referential or mutually recursive
    /// binding (e.g. `const x = x + 1`) fails with a diagnostic instead of
    /// overflowing the native stack — this can happen well before
    /// `MAX_EVALUATION_STEPS` steps accumulate, since each nesting level
    /// costs real Rust stack, not just a step count.
    fn eval_helper_value(
        &mut self,
        module: ModuleId,
        value: &Expression,
        span: Span,
    ) -> Option<Value> {
        if self.helper_eval_depth >= MAX_HELPER_EVAL_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                span,
                "compile-time evaluation recursed too deeply; check for a self-referential binding",
            ));
            return None;
        }
        self.helper_eval_depth += 1;
        let result = self.eval_expression(module, value, &mut Environment::new());
        self.helper_eval_depth -= 1;
        result
    }

    /// Applies a compile-time callable to an argument, bounded by
    /// `helper_eval_depth` so unbounded compile-time recursion (a
    /// self-referential `def`/`const`, or one whose base case can never be
    /// reached) fails with a diagnostic instead of overflowing the native
    /// stack. Evaluating a function's body is where recursive compile-time
    /// calls actually nest (each further call re-enters `apply_value`
    /// before the current one returns), so this — not the point where a
    /// bare `Expression::Function` node is wrapped into a `Value::Function`
    /// — is where depth needs to be tracked.
    fn apply_value(&mut self, callee: Value, argument: Value, span: Span) -> Option<Value> {
        if self.helper_eval_depth >= MAX_HELPER_EVAL_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                span,
                "compile-time evaluation recursed too deeply; check for a self-referential binding",
            ));
            return None;
        }
        self.helper_eval_depth += 1;
        let result = self.apply_value_inner(callee, argument, span);
        self.helper_eval_depth -= 1;
        result
    }

    fn apply_value_inner(&mut self, callee: Value, argument: Value, span: Span) -> Option<Value> {
        match callee {
            Value::Function {
                module,
                function,
                mut environment,
                quote_result,
            } => {
                if !bind_pattern(&function.pattern, argument, &mut environment) {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "compile-time function argument does not match its parameter",
                    ));
                    return None;
                }
                let previous_context = match quote_result {
                    Some(expected) => self.quote_context.replace(expected),
                    None => self.quote_context.take(),
                };
                let result = self.eval_expression(module, &function.body, &mut environment);
                self.quote_context = previous_context;
                result
            }
            Value::Helper(module, binding) => {
                let value = binding.value?;
                let previous_context = binding
                    .annotation
                    .as_ref()
                    .and_then(function_result_meta_type)
                    .map(|expected| self.quote_context.replace(expected))
                    .unwrap_or_else(|| self.quote_context.take());
                let function = self.eval_helper_value(module, &value, span.clone());
                self.quote_context = previous_context;
                // Unwrapping a helper into the function it names and then
                // applying that function is one logical call, not two — go
                // through `apply_value_inner` directly so `helper_eval_depth`
                // (and the resulting recursion-depth budget) tracks actual
                // call nesting rather than this implementation detail.
                self.apply_value_inner(function?, argument, span)
            }
            _ => {
                self.diagnostics
                    .push(Diagnostic::new(span, "cannot call this compile-time value"));
                None
            }
        }
    }

    /// Converts a compile-time `Value` produced by folding a `const`
    /// initializer back into a literal `Expression` the rest of the
    /// pipeline (resolve, typecheck, codegen) can treat like any other
    /// literal-valued binding. Only integers, finite floats, strings, and
    /// (possibly nested) products of those can be represented this way;
    /// anything else is reported as a diagnostic.
    fn value_to_expression(&mut self, value: Value, span: Span) -> Option<Expression> {
        match value {
            Value::Integer(integer) if integer >= 0 => {
                Some(Expression::Integer(crate::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span),
                    literal: integer.to_string(),
                }))
            }
            Value::Integer(integer) => {
                // Integer literals can never carry a leading `-` and the
                // language has no unary minus, so a negative compile-time
                // result is represented the same way the parser desugars
                // `0 - n`: `Subtract.subtract 0 |n|`.
                let zero = Expression::Integer(crate::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: "0".to_string(),
                });
                let magnitude = Expression::Integer(crate::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: integer.unsigned_abs().to_string(),
                });
                let access = Expression::Access(crate::AccessExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    value: Box::new(Expression::Name(crate::NameExpression {
                        syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                        name: "Subtract".to_string(),
                    })),
                    accessor: Accessor::Name("subtract".to_string()),
                });
                Some(Expression::Call(crate::CallExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    callee: Box::new(Expression::Call(crate::CallExpression {
                        syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                        callee: Box::new(access),
                        argument: Box::new(zero),
                    })),
                    argument: Box::new(magnitude),
                }))
            }
            Value::Float(float) if float.is_finite() && float >= 0.0 => {
                Some(Expression::Float(crate::FloatExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span),
                    literal: format!("{float:?}"),
                }))
            }
            // Mirrors the `Value::Integer` case above: float literals can
            // never carry a leading `-` either, so a negative compile-time
            // result is desugared the same way, via `Subtract.subtract`.
            Value::Float(float) if float.is_finite() => {
                let zero = Expression::Float(crate::FloatExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: "0.0".to_string(),
                });
                let magnitude = Expression::Float(crate::FloatExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: format!("{:?}", float.abs()),
                });
                let access = Expression::Access(crate::AccessExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    value: Box::new(Expression::Name(crate::NameExpression {
                        syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                        name: "Subtract".to_string(),
                    })),
                    accessor: Accessor::Name("subtract".to_string()),
                });
                Some(Expression::Call(crate::CallExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    callee: Box::new(Expression::Call(crate::CallExpression {
                        syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                        callee: Box::new(access),
                        argument: Box::new(zero),
                    })),
                    argument: Box::new(magnitude),
                }))
            }
            Value::Float(float) => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "compile-time float result `{float}` is not finite; \
                         `const` initializers must fold to a finite value"
                    ),
                ));
                None
            }
            // `Value::String`'s payload is already the raw, quoted source
            // literal (see `Expression::String`'s handling in
            // `eval_expression`, which clones `StringExpression::literal`
            // verbatim rather than decoding it) — reuse it as-is rather
            // than re-encoding, which would double-quote the result.
            Value::String(string) => Some(Expression::String(crate::StringExpression {
                syntax: Syntax::synthetic(self.fresh_id(), span),
                literal: string,
            })),
            Value::Product(fields) => {
                let elements = fields
                    .into_iter()
                    .map(|(name, value)| {
                        Some(crate::ProductElement {
                            syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                            name,
                            designated: false,
                            value: self.value_to_expression(value, span.clone())?,
                            spread: false,
                            named_spread: false,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(Expression::Product(crate::ProductExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span),
                    elements,
                }))
            }
            other => {
                let description = match other {
                    Value::Function { .. } | Value::Helper(..) => "a function value",
                    Value::Syntax(_) => "a syntax value",
                    Value::Sequence(_) => "a sequence value",
                    Value::Separated { .. } => "a separated sequence value",
                    Value::Nominal(..) => "a nominal value",
                    Value::Integer(_) | Value::Float(_) | Value::String(_) | Value::Product(_) => {
                        unreachable!()
                    }
                };
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "{description} cannot be represented as a compile-time constant; \
                         `const` initializers must fold to an integer, a float, a string, or a product of those"
                    ),
                ));
                None
            }
        }
    }

    /// Folds a builtin arithmetic/comparison operator call the parser
    /// desugars `+`/`-`/`*`/`/`/`==`/`!=`/`<`/`<=`/`>`/`>=` into (e.g.
    /// `Add.add left right` for `left + right`), the same way the language's
    /// former `infix` expression evaluator once folded operator chains
    /// directly by operator name. Returns `None` when `head`/`arguments`
    /// doesn't match one of these ten known trait/method pairs with exactly
    /// two arguments, so the caller falls back to ordinary call evaluation
    /// (this also means `..`/`..=`, which desugar to plain function calls
    /// rather than a trait method, are left to that fallback and remain
    /// unsupported at compile time, unchanged from before).
    fn eval_builtin_operator_call(
        &mut self,
        module: ModuleId,
        head: &Expression,
        arguments: &[&Expression],
        environment: &mut Environment,
    ) -> Option<Option<Value>> {
        const BUILTIN_OPERATOR_METHODS: [(&str, &str, &str); 10] = [
            ("Add", "add", "+"),
            ("Subtract", "subtract", "-"),
            ("Multiply", "multiply", "*"),
            ("Divide", "divide", "/"),
            ("Eq", "equal", "=="),
            ("Eq", "not_equal", "!="),
            ("PartialOrd", "lt", "<"),
            ("PartialOrd", "le", "<="),
            ("PartialOrd", "gt", ">"),
            ("PartialOrd", "ge", ">="),
        ];
        if arguments.len() != 2 {
            return None;
        }
        let Expression::Access(access) = head else {
            return None;
        };
        let Expression::Name(name) = access.value.as_ref() else {
            return None;
        };
        let Accessor::Name(method) = &access.accessor else {
            return None;
        };
        let operator = BUILTIN_OPERATOR_METHODS
            .iter()
            .find(|(trait_name, method_name, _)| *trait_name == name.name && *method_name == method)
            .map(|(_, _, operator)| *operator)?;
        let Some(left) = self.eval_expression(module, arguments[0], environment) else {
            return Some(None);
        };
        let Some(right) = self.eval_expression(module, arguments[1], environment) else {
            return Some(None);
        };
        Some(match (&left, &right, operator) {
            (Value::Integer(left), Value::Integer(right), "+") => {
                Some(Value::Integer(left.wrapping_add(*right)))
            }
            (Value::Integer(left), Value::Integer(right), "-") => {
                Some(Value::Integer(left.wrapping_sub(*right)))
            }
            (Value::Integer(left), Value::Integer(right), "*") => {
                Some(Value::Integer(left.wrapping_mul(*right)))
            }
            (Value::Integer(left), Value::Integer(right), "/") if *right != 0 => {
                Some(Value::Integer(left.wrapping_div(*right)))
            }
            (Value::Integer(left), Value::Integer(right), "==") => Some(bool_value(left == right)),
            (Value::Integer(left), Value::Integer(right), "!=") => Some(bool_value(left != right)),
            (Value::Integer(left), Value::Integer(right), "<") => Some(bool_value(left < right)),
            (Value::Integer(left), Value::Integer(right), "<=") => Some(bool_value(left <= right)),
            (Value::Integer(left), Value::Integer(right), ">") => Some(bool_value(left > right)),
            (Value::Integer(left), Value::Integer(right), ">=") => Some(bool_value(left >= right)),
            (Value::String(left), Value::String(right), "==") => Some(bool_value(left == right)),
            (Value::String(left), Value::String(right), "!=") => Some(bool_value(left != right)),
            (Value::Float(left), Value::Float(right), "+") => Some(Value::Float(left + right)),
            (Value::Float(left), Value::Float(right), "-") => Some(Value::Float(left - right)),
            (Value::Float(left), Value::Float(right), "*") => Some(Value::Float(left * right)),
            (Value::Float(left), Value::Float(right), "/") => Some(Value::Float(left / right)),
            (Value::Float(left), Value::Float(right), "==") => Some(bool_value(left == right)),
            (Value::Float(left), Value::Float(right), "!=") => Some(bool_value(left != right)),
            (Value::Float(left), Value::Float(right), "<") => Some(bool_value(left < right)),
            (Value::Float(left), Value::Float(right), "<=") => Some(bool_value(left <= right)),
            (Value::Float(left), Value::Float(right), ">") => Some(bool_value(left > right)),
            (Value::Float(left), Value::Float(right), ">=") => Some(bool_value(left >= right)),
            _ => {
                self.diagnostics.push(Diagnostic::new(
                    access.syntax.span.clone(),
                    format!("operator `{operator}` is not available for these compile-time values"),
                ));
                None
            }
        })
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

    fn instantiate_contextual_quote(
        &mut self,
        module: ModuleId,
        quote: &crate::QuoteExpression,
        environment: &Environment,
        expected: &MetaType,
    ) -> Option<Value> {
        let definitions = if quote.path.len() == 1 {
            let mut cursor = module;
            loop {
                if let Some(keys) = self.scopes[cursor.0].macros.get(&quote.path[0]) {
                    break Some(keys);
                }
                if !self.companion_modules[cursor.0] {
                    break None;
                }
                let Some(parent) = self.parent_modules[cursor.0] else {
                    break None;
                };
                cursor = parent;
            }
        } else {
            let namespace = quote.path[..quote.path.len() - 1].join(".");
            let mut cursor = module;
            let target = loop {
                if let Some(target) = self.scopes[cursor.0].namespaces.get(&namespace) {
                    break Some(*target);
                }
                if !self.companion_modules[cursor.0] {
                    break None;
                }
                let Some(parent) = self.parent_modules[cursor.0] else {
                    break None;
                };
                cursor = parent;
            };
            target.and_then(|target| self.scopes[target.0].macros.get(quote.path.last().unwrap()))
        };
        let matching = definitions
            .into_iter()
            .flatten()
            .filter(|key| {
                self.definitions.get(*key).is_some_and(|definition| {
                    definition.declaration.visibility == Visibility::Public || quote.path.len() == 1
                })
            })
            .collect::<Vec<_>>();
        if matching.len() != 1
            || !self.definitions.get(matching[0]).is_some_and(|definition| {
                matches!(
                    (&definition.kind, quote.kind),
                    (MacroKind::Quote, crate::QuoteKind::Quote)
                        | (MacroKind::ParseQuote, crate::QuoteKind::ParseQuote)
                )
            })
        {
            self.diagnostics.push(Diagnostic::new(
                quote.syntax.span.clone(),
                if quote.path.len() == 1 && matching.is_empty() {
                    format!(
                        "`{}` requires an explicit import from `std.syntax`",
                        quote.kind.name()
                    )
                } else {
                    format!(
                        "could not resolve quotation macro `{}`",
                        quote.path.join(".")
                    )
                },
            ));
            return None;
        }
        let definition = self.definitions[matching[0]].clone();
        self.record_invocation(quote.syntax.id, &definition);
        if *expected == MetaType::Syntax {
            let syntax = self.substitute_raw_quote(module, &quote.contents, environment)?;
            return Some(Value::Syntax(SyntaxValue::Raw(syntax)));
        }
        if quote_contains_raw_splice(&quote.contents, environment) {
            let syntax = self.substitute_raw_quote(module, &quote.contents, environment)?;
            return self.instantiate_substituted_fragment(module, &syntax, expected);
        }
        if *expected == MetaType::Type {
            return self.instantiate_type_quote(module, quote, environment);
        }
        if *expected == MetaType::Pattern {
            let mark = self.next_mark;
            self.next_mark += 1;
            let mut pattern = crate::parser::parse_pattern_template_fragment(
                &quote.contents,
                &mut self.next_syntax_id,
            )
            .map_err(|error| {
                self.diagnostics
                    .push(Diagnostic::new(quote.contents.span.clone(), error.message));
            })
            .ok()?;
            freshen_pattern(self, &mut pattern, module, mark);
            substitute_pattern(&mut pattern, environment, &mut self.diagnostics)?;
            return Some(Value::Syntax(SyntaxValue::Pattern(pattern)));
        }

        if matches!(expected, MetaType::Sequence(element) if **element == MetaType::Item)
            && quote
                .contents
                .tokens()
                .iter()
                .all(|token| token.kind.is_trivia())
        {
            return Some(Value::Syntax(SyntaxValue::Items(Vec::new())));
        }

        if *expected == MetaType::Visibility
            && quote
                .contents
                .tokens()
                .iter()
                .all(|token| token.kind.is_trivia())
        {
            return Some(Value::Syntax(SyntaxValue::Visibility(private_visibility())));
        }

        if matches!(expected, MetaType::Delimited(_, _))
            && !quote
                .contents
                .tokens()
                .iter()
                .any(|token| token.kind == crate::TokenKind::Dollar)
        {
            let value =
                delimiter_argument_value(expected, &quote.contents, &mut self.next_syntax_id);
            let Some(value) = value else {
                self.diagnostics.push(Diagnostic::new(
                    quote.contents.span.clone(),
                    format!(
                        "quotation cannot be interpreted as {}",
                        format_meta_type(expected)
                    ),
                ));
                return None;
            };
            return Some(Value::Syntax(value));
        }

        if matches!(
            expected,
            MetaType::SyntaxNode | MetaType::Comma | MetaType::Equals | MetaType::FatArrow
        ) && !quote
            .contents
            .tokens()
            .iter()
            .any(|token| token.kind == crate::TokenKind::Dollar)
        {
            let values = match_sequence_contents(
                &quote.contents,
                0,
                quote.contents.tokens().len(),
                expected,
                &mut self.next_syntax_id,
            );
            let Some(mut values) = values else {
                self.diagnostics.push(Diagnostic::new(
                    quote.contents.span.clone(),
                    "quotation does not contain exactly one structural syntax node",
                ));
                return None;
            };
            if values.len() != 1 {
                self.diagnostics.push(Diagnostic::new(
                    quote.contents.span.clone(),
                    "quotation does not contain exactly one structural syntax node",
                ));
                return None;
            }
            return Some(values.remove(0));
        }

        let mark = self.next_mark;
        self.next_mark += 1;
        let value = self.instantiate_quote(module, &quote.template, environment, mark)?;
        let valid = match expected {
            MetaType::SyntaxNode => !matches!(value, SyntaxValue::Items(_) | SyntaxValue::Raw(_)),
            MetaType::Expr => value.to_expression().is_some(),
            MetaType::Item => matches!(value, SyntaxValue::Item(_)),
            MetaType::Visibility => matches!(value, SyntaxValue::Visibility(_)),
            MetaType::Sequence(element) if **element == MetaType::Item => {
                matches!(value, SyntaxValue::Item(_) | SyntaxValue::Items(_))
            }
            _ => false,
        };
        if !valid {
            self.diagnostics.push(Diagnostic::new(
                quote.contents.span.clone(),
                format!(
                    "quotation cannot be interpreted as {}",
                    format_meta_type(expected)
                ),
            ));
            return None;
        }
        let value = if matches!(expected, MetaType::Sequence(element) if **element == MetaType::Item)
            && let SyntaxValue::Item(item) = value
        {
            SyntaxValue::Items(vec![*item])
        } else {
            value
        };
        Some(Value::Syntax(value))
    }

    fn instantiate_substituted_fragment(
        &mut self,
        module: ModuleId,
        syntax: &Syntax,
        expected: &MetaType,
    ) -> Option<Value> {
        let mark = self.next_mark;
        self.next_mark += 1;
        let value = match expected {
            MetaType::SyntaxNode => structural_syntax_value(syntax, &mut self.next_syntax_id)?,
            MetaType::Expr => {
                let mut expression =
                    crate::parser::parse_expression_fragment(syntax, &mut self.next_syntax_id)
                        .ok()?;
                alpha_rename_expression(&mut expression, mark, &mut Vec::new());
                self.freshen_expression(&mut expression, module, mark);
                SyntaxValue::from_expression(expression)
            }
            MetaType::Type => {
                let mut ty =
                    crate::parser::parse_type_fragment(syntax, false, &mut self.next_syntax_id)
                        .ok()?;
                freshen_type(self, &mut ty, module, mark);
                SyntaxValue::Type(ty)
            }
            MetaType::Pattern => {
                let mut pattern =
                    crate::parser::parse_pattern_fragment(syntax, false, &mut self.next_syntax_id)
                        .ok()?;
                freshen_pattern(self, &mut pattern, module, mark);
                SyntaxValue::Pattern(pattern)
            }
            MetaType::Item => {
                let mut item =
                    crate::parser::parse_item_fragment(syntax, &mut self.next_syntax_id).ok()?;
                alpha_rename_item(&mut item, mark);
                freshen_item(self, &mut item, module, mark);
                SyntaxValue::Item(Box::new(item))
            }
            MetaType::Sequence(element) if **element == MetaType::Item => {
                let mut items =
                    crate::parser::parse_item_list_fragment(syntax, &mut self.next_syntax_id)
                        .ok()?;
                for item in &mut items {
                    alpha_rename_item(item, mark);
                    freshen_item(self, item, module, mark);
                }
                SyntaxValue::Items(items)
            }
            _ => return None,
        };
        Some(Value::Syntax(value))
    }

    fn substitute_raw_quote(
        &mut self,
        module: ModuleId,
        contents: &Syntax,
        environment: &Environment,
    ) -> Option<Syntax> {
        let input = contents.tokens();
        let mut output = Vec::new();
        let mut cursor = 0;
        while cursor < input.len() {
            if input[cursor].kind != crate::TokenKind::Dollar {
                output.push(input[cursor].clone());
                cursor += 1;
                continue;
            }
            let mut name_at = cursor + 1;
            while name_at < input.len() && input[name_at].kind.is_trivia() {
                name_at += 1;
            }
            if name_at == input.len() || input[name_at].kind != crate::TokenKind::Identifier {
                output.push(input[cursor].clone());
                cursor += 1;
                continue;
            }
            let mut end = name_at + 1;
            let repeated = end < input.len() && input[end].kind == crate::TokenKind::Ellipsis;
            if repeated {
                end += 1;
            }
            let name = &input[name_at].text;
            let Some(value) = environment.get(name).map(EnvironmentBinding::get) else {
                self.diagnostics.push(Diagnostic::new(
                    contents.span.clone(),
                    format!("unknown syntax splice `${name}`"),
                ));
                return None;
            };
            match value {
                Value::Syntax(SyntaxValue::Items(items)) if repeated => {
                    for item in items {
                        output.extend_from_slice(item_syntax(&item).tokens());
                    }
                }
                Value::Sequence(values) if repeated => {
                    for value in values {
                        let Value::Syntax(value) = value else {
                            self.diagnostics.push(Diagnostic::new(
                                contents.span.clone(),
                                format!("repeated splice `${name}...` requires `Sequence Item`"),
                            ));
                            return None;
                        };
                        let syntax = value.syntax()?;
                        output.extend_from_slice(syntax.tokens());
                    }
                }
                Value::Syntax(value) if !repeated => {
                    let Some(syntax) = value.syntax() else {
                        self.diagnostics.push(Diagnostic::new(
                            contents.span.clone(),
                            format!("splice `${name}` contains an item sequence"),
                        ));
                        return None;
                    };
                    output.extend_from_slice(syntax.tokens());
                }
                _ => {
                    self.diagnostics.push(Diagnostic::new(
                        contents.span.clone(),
                        if repeated {
                            format!("repeated splice `${name}...` requires `Sequence Item`")
                        } else {
                            format!("splice `${name}` does not contain syntax")
                        },
                    ));
                    return None;
                }
            }
            cursor = end;
        }
        let mut offset = 0;
        let mut source = String::new();
        for token in &mut output {
            let start = offset;
            source.push_str(&token.text);
            offset += token.text.len();
            token.span = start..offset;
        }
        let source: Arc<str> = Arc::from(source);
        let mut syntax = contents.clone();
        syntax.id = self.fresh_id();
        syntax.tokens = Arc::from(output);
        syntax.token_range = 0..syntax.tokens.len();
        syntax.span = Span::User {
            source: Some(source),
            range: 0..offset,
            location: None,
        };
        let mark = self.next_mark;
        self.next_mark += 1;
        Some(syntax.generated(module.0, mark))
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
                for item in &mut loop_.body.items {
                    freshen_item(self, item, module, mark);
                }
            }
            Expression::Resource(resource) => {
                freshen_type(self, &mut resource.resource, module, mark);
            }
            Expression::With(with) => {
                freshen_type(self, &mut with.resource, module, mark);
                self.freshen_expression(&mut with.value, module, mark);
                self.freshen_syntax(&mut with.body.syntax, module, mark);
                for item in &mut with.body.items {
                    freshen_item(self, item, module, mark);
                }
            }
            Expression::Block(block) => {
                for item in &mut block.items {
                    freshen_item(self, item, module, mark);
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
            Expression::Logical(logical) => {
                self.freshen_expression(&mut logical.left, module, mark);
                self.freshen_expression(&mut logical.right, module, mark);
                freshen_type(self, &mut logical.bool_type, module, mark);
            }
            Expression::StringTemplate(template) => {
                for part in &mut template.parts {
                    if let crate::StringTemplatePart::Interpolation(interpolation) = part {
                        self.freshen_expression(&mut interpolation.expression, module, mark);
                    }
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

fn invalid_separated_parameter(expression: &Expression) -> bool {
    let mut current = expression;
    while let Expression::Function(function) = current {
        let ty = match &function.pattern {
            Pattern::Binding(binding) => Some(&binding.ty),
            Pattern::At(at) => Some(&at.binding.ty),
            Pattern::Wildcard(wildcard) => Some(&wildcard.ty),
            _ => None,
        };
        if ty.is_some_and(|ty| type_contains_named(ty, "Separated") && meta_type(ty).is_none()) {
            return true;
        }
        current = &function.body;
    }
    false
}

fn validate_top_level_sequence(definition: &MacroDefinition, diagnostics: &mut Vec<Diagnostic>) {
    let positions = definition
        .parameters
        .iter()
        .enumerate()
        .filter_map(|(index, parameter)| {
            matches!(parameter, MetaType::Sequence(_)).then_some(index)
        })
        .collect::<Vec<_>>();
    if positions.is_empty() {
        return;
    }
    if definition.key.modifier {
        diagnostics.push(Diagnostic::new(
            definition.declaration.syntax.span.clone(),
            "top-level `Sequence` parameters are not supported by modifier macros",
        ));
        return;
    }
    if positions.len() > 1 {
        diagnostics.push(Diagnostic::new(
            definition.declaration.syntax.span.clone(),
            "a macro signature may contain at most one top-level `Sequence` parameter",
        ));
        return;
    }
    let position = positions[0];
    let MetaType::Sequence(element) = &definition.parameters[position] else {
        unreachable!();
    };
    if matches!(
        element.as_ref(),
        MetaType::Sequence(_) | MetaType::Optional(_) | MetaType::Product(_)
    ) {
        diagnostics.push(Diagnostic::new(
            definition.declaration.syntax.span.clone(),
            format!(
                "a top-level `Sequence` element must be a single syntax category, found `{}`",
                format_meta_type(element)
            ),
        ));
        return;
    }
    if !definition.parameters[position + 1..]
        .iter()
        .any(meta_type_guarantees_source_consumption)
    {
        diagnostics.push(Diagnostic::new(
            definition.declaration.syntax.span.clone(),
            "a top-level `Sequence` parameter must be followed by a parameter that always consumes source syntax",
        ));
    }
}

fn meta_type_guarantees_source_consumption(meta: &MetaType) -> bool {
    !matches!(meta, MetaType::Visibility | MetaType::MacroCallVisibility)
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

fn function_result_meta_type(mut annotation: &Type) -> Option<MetaType> {
    while let Type::Function(function) = annotation {
        annotation = &function.result;
    }
    meta_type(annotation).filter(quote_result_type)
}

fn meta_type(ty: &Type) -> Option<MetaType> {
    match ty {
        Type::Named(named) if named.namespace.is_none() => match named.name.as_str() {
            "Syntax" => Some(MetaType::Syntax),
            "SyntaxNode" => Some(MetaType::SyntaxNode),
            "Expr" => Some(MetaType::Expr),
            "Ident" => Some(MetaType::Ident(None)),
            "CallExpr" => Some(MetaType::CallExpr),
            "StringExpr" => Some(MetaType::StringExpr),
            "UnstructuredExpr" => Some(MetaType::UnstructuredExpr),
            "Type" => Some(MetaType::Type),
            "Pattern" => Some(MetaType::Pattern),
            "BindingPattern" => Some(MetaType::BindingPattern),
            "NominalPattern" => Some(MetaType::NominalPattern),
            "Item" => Some(MetaType::Item),
            "TypeDeclarationItem" => Some(MetaType::TypeDeclarationItem),
            "UnstructuredItem" => Some(MetaType::UnstructuredItem),
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
    // A single element doesn't need the grouping parens that separate
    // multiple fixed contents, so `Parenthesized Expr` is read the same as
    // `Parenthesized (Expr)`.
    let Type::Product(product) = ty else {
        return meta_type(ty).map(|single| DelimitedMetaContents::Fixed(vec![single]));
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
        [element] => meta_type(unwrap_singleton_product(element)),
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
        MetaType::Type => {
            let mut next_syntax_id = 0;
            parse_type_argument(argument, &mut next_syntax_id).is_some()
        }
        MetaType::Pattern | MetaType::BindingPattern | MetaType::NominalPattern => {
            let mut next_syntax_id = 0;
            parse_pattern_argument(argument, &mut next_syntax_id).is_some_and(|pattern| {
                matches!(expected, MetaType::Pattern)
                    || matches!(expected, MetaType::BindingPattern)
                        && matches!(pattern, Pattern::Binding(_))
                    || matches!(expected, MetaType::NominalPattern)
                        && matches!(pattern, Pattern::Nominal(_))
            })
        }
        MetaType::Visibility => matches!(argument, Expression::VisibilityArgument(_)),
        MetaType::MacroCallVisibility => false,
        MetaType::Syntax => true,
        MetaType::SyntaxNode => !matches!(argument, Expression::SyntaxArgument(_)),
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
        MetaType::StringExpr => matches!(expression_argument, Expression::String(_)),
        MetaType::UnstructuredExpr => !matches!(
            expression_argument,
            Expression::Name(_) | Expression::Call(_) | Expression::VisibilityArgument(_)
        ),
        MetaType::Item
        | MetaType::TypeDeclarationItem
        | MetaType::UnstructuredItem
        | MetaType::Product(_)
        | MetaType::Optional(_)
        | MetaType::Sequence(_) => false,
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
        DelimitedMetaContents::Fixed(elements) if elements.as_slice() == [MetaType::Syntax] => {
            Some(DelimitedValueContents::Fixed(vec![(
                None,
                Value::Syntax(SyntaxValue::Raw(syntax_slice(syntax, first + 1, last))),
            )]))
        }
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
        MetaType::Expr | MetaType::CallExpr | MetaType::StringExpr | MetaType::UnstructuredExpr => {
            let (value, ids) = expression()?;
            let value = SyntaxValue::from_expression(value);
            if matches!(expected, MetaType::CallExpr) && !matches!(value, SyntaxValue::Call(_))
                || matches!(expected, MetaType::StringExpr)
                    && !matches!(value, SyntaxValue::Unstructured(Expression::String(_)))
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
        MetaType::Pattern | MetaType::BindingPattern | MetaType::NominalPattern => {
            let pattern =
                crate::parser::parse_pattern_fragment(syntax, false, next_syntax_id).ok()?;
            if matches!(expected, MetaType::BindingPattern)
                && !matches!(pattern, Pattern::Binding(_))
                || matches!(expected, MetaType::NominalPattern)
                    && !matches!(pattern, Pattern::Nominal(_))
            {
                return None;
            }
            SyntaxValue::Pattern(pattern)
        }
        MetaType::Item | MetaType::TypeDeclarationItem | MetaType::UnstructuredItem => {
            let item = crate::parser::parse_item_fragment(syntax, next_syntax_id).ok()?;
            if matches!(expected, MetaType::TypeDeclarationItem)
                && !matches!(item, Item::TypeDeclaration(_))
                || matches!(expected, MetaType::UnstructuredItem)
                    && matches!(item, Item::TypeDeclaration(_))
            {
                return None;
            }
            SyntaxValue::Item(Box::new(item))
        }
        MetaType::Visibility | MetaType::MacroCallVisibility => {
            let (value, ids) = expression()?;
            let Expression::VisibilityArgument(value) = value else {
                return None;
            };
            *next_syntax_id = ids;
            SyntaxValue::Visibility(value)
        }
        MetaType::Syntax => SyntaxValue::Raw(syntax.clone()),
        MetaType::SyntaxNode => structural_syntax_value(syntax, next_syntax_id)?,
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
            DelimitedMetaContents::Sequence(Box::new(MetaType::SyntaxNode)),
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
) -> Option<(Vec<MacroArgument>, usize, Vec<(MetaType, bool)>)> {
    let mut matched = Vec::with_capacity(definition.parameters.len());
    let mut effective = Vec::new();
    let mut parameter_index = 0;

    if definition.parameters.first() == Some(&MetaType::MacroCallVisibility) {
        matched.push(MacroArgument::Visibility(
            call_visibility.cloned().unwrap_or_else(private_visibility),
        ));
        parameter_index += 1;
    } else if call_visibility.is_some() {
        return None;
    }

    let consumed = match_macro_parameter_suffix(
        &definition.parameters[parameter_index..],
        arguments,
        0,
        &mut matched,
        &mut effective,
    )?;
    Some((matched, consumed, effective))
}

fn match_macro_parameter_suffix(
    parameters: &[MetaType],
    arguments: &[&Expression],
    argument_index: usize,
    matched: &mut Vec<MacroArgument>,
    effective: &mut Vec<(MetaType, bool)>,
) -> Option<usize> {
    let Some((expected, rest)) = parameters.split_first() else {
        return Some(argument_index);
    };
    match expected {
        MetaType::Visibility => {
            let matched_len = matched.len();
            let effective_len = effective.len();
            if let Some(Expression::VisibilityArgument(visibility)) =
                arguments.get(argument_index).copied()
            {
                matched.push(MacroArgument::Visibility(visibility.clone()));
                effective.push((MetaType::Visibility, false));
                if let Some(consumed) = match_macro_parameter_suffix(
                    rest,
                    arguments,
                    argument_index + 1,
                    matched,
                    effective,
                ) {
                    return Some(consumed);
                }
                matched.truncate(matched_len);
                effective.truncate(effective_len);
            }
            matched.push(MacroArgument::Visibility(private_visibility()));
            let result =
                match_macro_parameter_suffix(rest, arguments, argument_index, matched, effective);
            if result.is_none() {
                matched.truncate(matched_len);
                effective.truncate(effective_len);
            }
            result
        }
        MetaType::Sequence(element) => {
            let mut maximum = argument_index;
            while arguments
                .get(maximum)
                .is_some_and(|argument| meta_type_matches(element, argument))
            {
                maximum += 1;
            }
            for end in (argument_index..=maximum).rev() {
                let matched_len = matched.len();
                let effective_len = effective.len();
                matched.push(MacroArgument::Sequence(
                    arguments[argument_index..end]
                        .iter()
                        .map(|argument| (*argument).clone())
                        .collect(),
                ));
                effective.extend((argument_index..end).map(|_| ((**element).clone(), true)));
                if let Some(consumed) =
                    match_macro_parameter_suffix(rest, arguments, end, matched, effective)
                {
                    return Some(consumed);
                }
                matched.truncate(matched_len);
                effective.truncate(effective_len);
            }
            None
        }
        MetaType::MacroCallVisibility => None,
        _ => {
            let argument = arguments.get(argument_index).copied()?;
            if !meta_type_matches(expected, argument) {
                return None;
            }
            let matched_len = matched.len();
            let effective_len = effective.len();
            matched.push(MacroArgument::Expression(argument.clone()));
            effective.push((expected.clone(), false));
            let result = match_macro_parameter_suffix(
                rest,
                arguments,
                argument_index + 1,
                matched,
                effective,
            );
            if result.is_none() {
                matched.truncate(matched_len);
                effective.truncate(effective_len);
            }
            result
        }
    }
}

fn private_visibility() -> VisibilitySyntax {
    VisibilitySyntax {
        syntax: Syntax::compiler(),
        kind: VisibilityKind::Private,
    }
}

/// Unwraps a modifier's declared leading-argument type, which must be
/// spelled `Parenthesized (...)` to match how every other delimited macro
/// argument is declared, down to the single meta type it wraps.
fn modifier_argument_meta_type(declared: &MetaType) -> Option<&MetaType> {
    let MetaType::Delimited(DelimiterKind::Parenthesized, DelimitedMetaContents::Fixed(inner)) =
        declared
    else {
        return None;
    };
    match inner.as_slice() {
        [expected] => Some(expected),
        _ => None,
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
        | MetaType::SyntaxNode
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

/// Reinterprets an expression-shaped macro argument as type syntax.
///
/// Parentheses normally delimit a compound category argument and are removed.
/// If their contents are not a complete type, retry with the parentheses as
/// part of the type so product types can use their natural spelling.
fn parse_type_argument(argument: &Expression, next_syntax_id: &mut usize) -> Option<Type> {
    let (syntax, grouped) = category_argument_syntax(argument)?;
    if grouped && let Ok(ty) = crate::parser::parse_type_fragment(syntax, true, next_syntax_id) {
        return Some(ty);
    }
    crate::parser::parse_type_fragment(syntax, false, next_syntax_id).ok()
}

/// Reinterprets an expression-shaped macro argument as pattern syntax.
///
/// As with types, preserve the existing grouping interpretation when it is
/// valid, then retain the parentheses when they form the pattern itself. This
/// lets `(left, right)` be captured directly as a product pattern while keeping
/// `((left, right))` and `(Some value)` compatible with existing macros.
fn parse_pattern_argument(argument: &Expression, next_syntax_id: &mut usize) -> Option<Pattern> {
    let (syntax, grouped) = category_argument_syntax(argument)?;
    if grouped
        && let Ok(pattern) = crate::parser::parse_pattern_fragment(syntax, true, next_syntax_id)
    {
        return Some(pattern);
    }
    crate::parser::parse_pattern_fragment(syntax, false, next_syntax_id).ok()
}

fn meta_argument_expression<'a>(expected: &MetaType, argument: &'a Expression) -> &'a Expression {
    if matches!(
        expected,
        MetaType::Syntax | MetaType::SyntaxNode | MetaType::Expr
    ) {
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
        || matches!(right, MetaType::SyntaxNode) && !matches!(left, MetaType::Syntax)
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

fn effective_signature_more_specific(
    left: &[(MetaType, bool)],
    right: &[(MetaType, bool)],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(
            |((left_type, left_repeated), (right_type, right_repeated))| {
                meta_type_at_least_as_specific(left_type, right_type)
                    && (!left_repeated || *right_repeated)
            },
        )
        && left != right
}

fn format_meta_signature(parameters: &[MetaType]) -> String {
    parameters
        .iter()
        .map(|parameter| match parameter {
            MetaType::Syntax => "Syntax".to_owned(),
            MetaType::SyntaxNode => "SyntaxNode".to_owned(),
            MetaType::Expr => "Expr".to_owned(),
            MetaType::Ident(None) => "Ident String".to_owned(),
            MetaType::Ident(Some(spelling)) => format!("Ident {spelling:?}"),
            MetaType::CallExpr => "CallExpr".to_owned(),
            MetaType::StringExpr => "StringExpr".to_owned(),
            MetaType::UnstructuredExpr => "UnstructuredExpr".to_owned(),
            MetaType::Type => "Type".to_owned(),
            MetaType::Pattern => "Pattern".to_owned(),
            MetaType::BindingPattern => "BindingPattern".to_owned(),
            MetaType::NominalPattern => "NominalPattern".to_owned(),
            MetaType::Item => "Item".to_owned(),
            MetaType::TypeDeclarationItem => "TypeDeclarationItem".to_owned(),
            MetaType::UnstructuredItem => "UnstructuredItem".to_owned(),
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
    if matches!(definition.kind, MacroKind::Quote | MacroKind::ParseQuote)
        && let Some(annotation) = &definition.declaration.annotation
    {
        let parameters = definition
            .declaration
            .type_parameters
            .iter()
            .flat_map(crate::TypeParameterPattern::names)
            .collect::<Vec<_>>();
        let bounds = definition
            .declaration
            .trait_bounds
            .iter()
            .map(|bound| {
                let arguments = bound
                    .arguments
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{} {arguments}", bound.trait_name.name)
            })
            .collect::<Vec<_>>();
        let generic = if parameters.is_empty() {
            String::new()
        } else if bounds.is_empty() {
            format!("<{}> ", parameters.join(", "))
        } else {
            format!("<{} where {}> ", parameters.join(", "), bounds.join(", "))
        };
        return ResolvedMacro {
            declaration: definition.declaration.syntax.id,
            name: definition.key.name.clone(),
            modifier: false,
            signature: format!("{generic}{annotation}"),
            docs: definition.declaration.docs.clone(),
        };
    }
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
        docs: definition.declaration.docs.clone(),
    }
}

pub(crate) fn format_meta_type(meta: &MetaType) -> String {
    match meta {
        MetaType::Syntax => "Syntax".to_owned(),
        MetaType::SyntaxNode => "SyntaxNode".to_owned(),
        MetaType::Expr => "Expr".to_owned(),
        MetaType::Ident(None) => "Ident String".to_owned(),
        MetaType::Ident(Some(spelling)) => format!("Ident {spelling:?}"),
        MetaType::CallExpr => "CallExpr".to_owned(),
        MetaType::StringExpr => "StringExpr".to_owned(),
        MetaType::UnstructuredExpr => "UnstructuredExpr".to_owned(),
        MetaType::Type => "Type".to_owned(),
        MetaType::Pattern => "Pattern".to_owned(),
        MetaType::BindingPattern => "BindingPattern".to_owned(),
        MetaType::NominalPattern => "NominalPattern".to_owned(),
        MetaType::Item => "Item".to_owned(),
        MetaType::TypeDeclarationItem => "TypeDeclarationItem".to_owned(),
        MetaType::UnstructuredItem => "UnstructuredItem".to_owned(),
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

fn compiler_macro_signature(annotation: &Type) -> Option<(Vec<MetaType>, MetaType)> {
    let Type::Function(function) = annotation else {
        return None;
    };
    Some((
        vec![meta_type(&function.parameter)?],
        meta_type(&function.result).unwrap_or(MetaType::Syntax),
    ))
}

fn inferred_macro_signature(value: Option<&Expression>) -> (Vec<MetaType>, MetaType) {
    let mut parameters = Vec::new();
    let mut current = value;
    while let Some(Expression::Function(function)) = current {
        parameters.push(pattern_meta_type(&function.pattern).unwrap_or(MetaType::SyntaxNode));
        current = Some(&function.body);
    }
    (
        parameters,
        current
            .map(inferred_result_meta_type)
            .unwrap_or(MetaType::Syntax),
    )
}

fn inferred_result_meta_type(expression: &Expression) -> MetaType {
    match expression {
        // `quote` always returns opaque `Syntax`, regardless of what its
        // contents would otherwise parse as; only `parse_quote` infers a
        // result from the quoted template's shape.
        Expression::Quote(quote) if quote.kind == crate::QuoteKind::Quote => MetaType::Syntax,
        Expression::Quote(quote) => match &quote.template {
            crate::QuoteTemplate::Expression(_) => MetaType::Expr,
            crate::QuoteTemplate::Item(_) => MetaType::Item,
            crate::QuoteTemplate::Items(_) => MetaType::Sequence(Box::new(MetaType::Item)),
            crate::QuoteTemplate::Raw => MetaType::Syntax,
        },
        Expression::Block(block) => block
            .items
            .last()
            .and_then(|item| match item {
                Item::Expression(expression) => Some(inferred_result_meta_type(expression)),
                Item::Return(return_) => Some(inferred_result_meta_type(&return_.value)),
                _ => None,
            })
            .unwrap_or(MetaType::SyntaxNode),
        Expression::Match(match_) => match_
            .arms
            .first()
            .map(|arm| inferred_result_meta_type(&arm.body))
            .unwrap_or(MetaType::SyntaxNode),
        _ => MetaType::SyntaxNode,
    }
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

pub(crate) fn pattern_meta_type(pattern: &Pattern) -> Option<MetaType> {
    match pattern {
        Pattern::At(at) => match &at.binding.ty {
            Type::Inferred(_) => pattern_meta_type(&at.pattern),
            ty => meta_type(ty),
        },
        Pattern::Binding(binding) => match &binding.ty {
            Type::Inferred(_) => Some(MetaType::SyntaxNode),
            ty => meta_type(ty),
        },
        Pattern::Wildcard(wildcard) => match &wildcard.ty {
            Type::Inferred(_) => Some(MetaType::SyntaxNode),
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
                | "SyntaxNode"
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
                || function.effects.resources.iter().any(type_contains_syntax)
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
                    .effects
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
                    .effects
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
        Pattern::At(at) => {
            type_contains_syntax(&at.binding.ty) || pattern_contains_syntax(&at.pattern)
        }
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
        Expression::Logical(logical) => {
            obviously_not_syntax(&logical.left, 0) || obviously_not_syntax(&logical.right, 0)
        }
        Expression::StringTemplate(_) => true,
        Expression::Loop(_) => true,
        Expression::Resource(_) | Expression::With(_) => true,
        Expression::Block(block) => block.items.last().is_none_or(|item| match item {
            Item::Expression(expression) => obviously_not_syntax(expression, 0),
            Item::Return(return_) => obviously_not_syntax(&return_.value, 0),
            Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Submodule(_)
            | Item::TypeDeclaration(_)
            | Item::UseDeclaration(_) => true,
            _ => true,
        }),
        Expression::Function(_)
        | Expression::Product(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => true,
    }
}

/// True when a macro body's tail position — through curried parameters,
/// `satisfies`, `match` arms, and a block's trailing expression — is a bare
/// `quote { ... }`. `quote` always produces opaque `Syntax`, so it can never
/// satisfy a concrete syntax result type such as `Expr`.
fn quote_at_tail(expression: &Expression, arity: usize) -> bool {
    if arity > 0 {
        return match expression {
            Expression::Function(function) => quote_at_tail(&function.body, arity - 1),
            Expression::Satisfies(satisfies) => quote_at_tail(&satisfies.value, arity),
            _ => false,
        };
    }
    match expression {
        Expression::Quote(quote) => quote.kind == crate::QuoteKind::Quote,
        Expression::Satisfies(satisfies) => quote_at_tail(&satisfies.value, 0),
        Expression::Match(match_) => match_.arms.iter().any(|arm| quote_at_tail(&arm.body, 0)),
        Expression::Block(block) => block.items.last().is_some_and(|item| match item {
            Item::Expression(expression) => quote_at_tail(expression, 0),
            Item::Return(return_) => quote_at_tail(&return_.value, 0),
            _ => false,
        }),
        _ => false,
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

fn invalid_raw_syntax_shape(meta: &MetaType) -> bool {
    match meta {
        MetaType::Delimited(_, DelimitedMetaContents::Fixed(elements))
            if elements.as_slice() == [MetaType::Syntax] =>
        {
            false
        }
        MetaType::Delimited(_, DelimitedMetaContents::Fixed(elements))
        | MetaType::Product(elements) => elements.iter().any(|element| {
            matches!(element, MetaType::Syntax) || invalid_raw_syntax_shape(element)
        }),
        MetaType::Delimited(_, DelimitedMetaContents::Sequence(element))
        | MetaType::Sequence(element)
        | MetaType::Optional(element) => {
            matches!(element.as_ref(), MetaType::Syntax) || invalid_raw_syntax_shape(element)
        }
        MetaType::Delimited(_, DelimitedMetaContents::Separated { element, separator }) => {
            matches!(element.as_ref(), MetaType::Syntax)
                || matches!(separator.as_ref(), MetaType::Syntax)
                || invalid_raw_syntax_shape(element)
                || invalid_raw_syntax_shape(separator)
        }
        _ => false,
    }
}

/// Contextual syntax types that `parse_quote` can use to interpret its input.
/// This is independent of the intrinsic macro's declared `Syntax` output.
/// Excludes `Syntax`: an opaque, unparsed fragment is `quote`'s result, not a
/// syntax node `parse_quote` can construct contextually.
fn quote_result_type(meta: &MetaType) -> bool {
    match meta {
        MetaType::SyntaxNode
        | MetaType::Expr
        | MetaType::Type
        | MetaType::Pattern
        | MetaType::Item
        | MetaType::Comma
        | MetaType::Equals
        | MetaType::FatArrow
        | MetaType::Visibility => true,
        MetaType::Sequence(element) => **element == MetaType::Item,
        MetaType::Delimited(_, _) => !invalid_raw_syntax_shape(meta),
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
        Pattern::At(at) => {
            if !matches_pattern_type(&at.binding.ty, &value) {
                return false;
            }
            environment.insert(
                at.binding.name.clone(),
                EnvironmentBinding::new(value.clone(), at.binding.mutable),
            );
            bind_pattern(&at.pattern, value, environment)
        }
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
            // A single unnamed, non-spread element in parentheses is
            // grouping, not a genuine 1-element tuple, matching the
            // transparency rule the type checker applies to the same shape
            // (e.g. `typecheck.rs`'s `product.elements.len() == 1 &&
            // !product.elements[0].spread` checks) — so a call argument
            // like `f (a - b)` binds `f`'s plain parameter directly to the
            // result of `a - b` rather than to a product wrapping it. Only
            // a bare binding pattern unwraps this way; `Pattern::Product`
            // still requires a genuine product/sequence value below.
            let value = match value {
                Value::Product(mut fields) if fields.len() == 1 && fields[0].0.is_none() => {
                    fields.pop().expect("length checked above").1
                }
                other => other,
            };
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
                && pattern.name == "TypeDeclarationItem"
                && let Value::Syntax(SyntaxValue::Item(item)) = &value
                && let Item::TypeDeclaration(declaration) = item.as_ref()
            {
                return bind_pattern(
                    &pattern.argument,
                    type_declaration_item_value(declaration),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && pattern.name == "UnstructuredItem"
                && let Value::Syntax(SyntaxValue::Item(item)) = &value
                && !matches!(item.as_ref(), Item::TypeDeclaration(_))
            {
                return bind_pattern(&pattern.argument, Value::Product(Vec::new()), environment);
            }
            if pattern.namespace.is_none()
                && let Some(kind) = delimiter_kind(&pattern.name)
                && let Value::Syntax(SyntaxValue::Delimited(delimited)) = &value
                && delimited.kind == kind
            {
                let contents = match &delimited.contents {
                    DelimitedValueContents::Fixed(values)
                        if matches!(
                            values.as_slice(),
                            [(None, Value::Syntax(SyntaxValue::Raw(_)))]
                        ) =>
                    {
                        values[0].1.clone()
                    }
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

fn type_declaration_item_value(declaration: &crate::TypeDeclaration) -> Value {
    let kind = match declaration.kind {
        crate::TypeDeclarationKind::Alias => "AliasDeclaration",
        crate::TypeDeclarationKind::Distinct => "DistinctDeclaration",
        crate::TypeDeclarationKind::Singleton => "SingletonDeclaration",
        crate::TypeDeclarationKind::Opaque => "OpaqueDeclaration",
    };
    let identifier = |name: &str| {
        let mut syntax = if name == declaration.name {
            declaration.name_syntax.clone()
        } else {
            declaration.syntax.clone()
        };
        if let Some(index) = syntax
            .tokens()
            .iter()
            .position(|token| token.kind == crate::TokenKind::Identifier && token.text == name)
        {
            let start = syntax.token_range.start + index;
            syntax.token_range = start..start + 1;
        }
        Value::Syntax(SyntaxValue::Ident(crate::NameExpression {
            syntax,
            name: name.to_owned(),
        }))
    };
    let parameters = declaration
        .type_parameters
        .iter()
        .flat_map(crate::TypeParameterPattern::names)
        .map(identifier)
        .collect();
    let mut declared_type = Type::Named(crate::NamedType {
        syntax: declaration.name_syntax.clone(),
        namespace: None,
        name: declaration.name.clone(),
    });
    for parameter in &declaration.type_parameters {
        let argument = type_parameter_pattern_type(parameter);
        declared_type = Type::Application(crate::TypeApplication {
            syntax: declaration.syntax.clone(),
            callee: Box::new(declared_type),
            argument: Box::new(argument),
        });
    }
    let underlying = match &declaration.underlying {
        Some(ty) => Value::Nominal(
            "Some".to_owned(),
            Box::new(Value::Syntax(SyntaxValue::Type(ty.clone()))),
        ),
        None => Value::Nominal("None".to_owned(), Box::new(Value::Product(Vec::new()))),
    };
    Value::Product(vec![
        (
            Some("kind".to_owned()),
            Value::Nominal(kind.to_owned(), Box::new(Value::Product(Vec::new()))),
        ),
        (Some("name".to_owned()), identifier(&declaration.name)),
        (
            Some("name_spelling".to_owned()),
            Value::String(declaration.name.clone()),
        ),
        (
            Some("declared_type".to_owned()),
            Value::Syntax(SyntaxValue::Type(declared_type)),
        ),
        (
            Some("type_parameters".to_owned()),
            Value::Sequence(parameters),
        ),
        (Some("underlying".to_owned()), underlying),
    ])
}

fn type_parameter_pattern_type(parameter: &crate::TypeParameterPattern) -> Type {
    match parameter {
        crate::TypeParameterPattern::Binding(binding) => Type::Named(crate::NamedType {
            syntax: binding.syntax.clone(),
            namespace: None,
            name: binding.name.clone(),
        }),
        crate::TypeParameterPattern::Effect(binding) => Type::Named(crate::NamedType {
            syntax: binding.syntax.clone(),
            namespace: None,
            name: binding.name.clone(),
        }),
        crate::TypeParameterPattern::Product(product) => Type::Product(crate::ProductType {
            syntax: product.syntax.clone(),
            elements: product
                .elements
                .iter()
                .map(|element| crate::TypeElement {
                    syntax: element.syntax().clone(),
                    name: None,
                    ty: type_parameter_pattern_type(element),
                    spread: false,
                    mutable: false,
                })
                .collect(),
            variadic: false,
        }),
        crate::TypeParameterPattern::Splice(splice) => Type::Splice(splice.clone()),
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
        (MetaType::Syntax, Value::Syntax(SyntaxValue::Raw(_))) => true,
        (MetaType::SyntaxNode, Value::Syntax(value)) => !matches!(value, SyntaxValue::Raw(_)),
        (MetaType::Expr, Value::Syntax(syntax)) => syntax.to_expression().is_some(),
        (MetaType::Ident(spelling), Value::Syntax(SyntaxValue::Ident(name))) => spelling
            .as_ref()
            .is_none_or(|expected| expected == &name.name),
        (MetaType::CallExpr, Value::Syntax(SyntaxValue::Call(_))) => true,
        (MetaType::StringExpr, Value::Syntax(SyntaxValue::Unstructured(Expression::String(_)))) => {
            true
        }
        (MetaType::UnstructuredExpr, Value::Syntax(SyntaxValue::Unstructured(_))) => true,
        (MetaType::Type, Value::Syntax(SyntaxValue::Type(_))) => true,
        (MetaType::Pattern, Value::Syntax(SyntaxValue::Pattern(_))) => true,
        (MetaType::BindingPattern, Value::Syntax(SyntaxValue::Pattern(Pattern::Binding(_)))) => {
            true
        }
        (MetaType::NominalPattern, Value::Syntax(SyntaxValue::Pattern(Pattern::Nominal(_)))) => {
            true
        }
        (MetaType::Item, Value::Syntax(SyntaxValue::Item(_))) => true,
        (MetaType::TypeDeclarationItem, Value::Syntax(SyntaxValue::Item(item))) => {
            matches!(item.as_ref(), Item::TypeDeclaration(_))
        }
        (MetaType::UnstructuredItem, Value::Syntax(SyntaxValue::Item(item))) => {
            !matches!(item.as_ref(), Item::TypeDeclaration(_))
        }
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
        (MetaType::Product(expected), Value::Sequence(values)) => {
            expected.len() == values.len()
                && expected
                    .iter()
                    .zip(values)
                    .all(|(expected, value)| meta_type_matches_value(expected, value))
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
            for item in &mut loop_.body.items {
                substitute_block_item(item, environment, diagnostics)?;
            }
        }
        Expression::Resource(resource) => {
            substitute_type(&mut resource.resource, environment, diagnostics)?;
        }
        Expression::With(with) => {
            substitute_type(&mut with.resource, environment, diagnostics)?;
            *with.value = substitute_splices(&with.value, environment, diagnostics)?;
            for item in &mut with.body.items {
                substitute_block_item(item, environment, diagnostics)?;
            }
        }
        Expression::Block(block) => {
            for item in &mut block.items {
                substitute_block_item(item, environment, diagnostics)?;
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
        Expression::Logical(logical) => {
            *logical.left = substitute_splices(&logical.left, environment, diagnostics)?;
            *logical.right = substitute_splices(&logical.right, environment, diagnostics)?;
            substitute_type(&mut logical.bool_type, environment, diagnostics)?;
        }
        Expression::StringTemplate(template) => {
            for part in &mut template.parts {
                if let crate::StringTemplatePart::Interpolation(interpolation) = part {
                    *interpolation.expression =
                        substitute_splices(&interpolation.expression, environment, diagnostics)?;
                }
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

fn substitute_block_item(
    item: &mut Item,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    match item {
        Item::Binding(binding) => {
            substitute_binding(binding, environment, diagnostics)?;
        }
        Item::PatternBinding(binding) => {
            substitute_pattern(&mut binding.pattern, environment, diagnostics)?;
            binding.value = substitute_splices(&binding.value, environment, diagnostics)?
        }
        Item::Assignment(assignment) => {
            assignment.target = substitute_splices(&assignment.target, environment, diagnostics)?;
            assignment.value = substitute_splices(&assignment.value, environment, diagnostics)?;
        }
        Item::Return(return_) => {
            return_.value = substitute_splices(&return_.value, environment, diagnostics)?
        }
        Item::Break(break_) => {
            if let Some(value) = &mut break_.value {
                *value = substitute_splices(value, environment, diagnostics)?;
            }
        }
        Item::Continue(_) => {}
        Item::Expression(expression) => {
            *expression = substitute_splices(expression, environment, diagnostics)?
        }
        Item::Submodule(submodule) => {
            substitute_identifier(
                &mut submodule.name,
                &mut submodule.syntax,
                environment,
                diagnostics,
            )?;
            substitute_item_list(&mut submodule.module.items, environment, diagnostics)?;
        }
        Item::TypeDeclaration(declaration) => {
            substitute_identifier(
                &mut declaration.name,
                &mut declaration.syntax,
                environment,
                diagnostics,
            )?;
            substitute_type_parameter_list(
                &mut declaration.type_parameters,
                environment,
                diagnostics,
            )?;
            for bound in &mut declaration.trait_bounds {
                substitute_trait_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.subtype_bounds {
                substitute_subtype_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.default_bounds {
                substitute_default_bound(bound, environment, diagnostics)?;
            }
            if let Some(underlying) = &mut declaration.underlying {
                substitute_type(underlying, environment, diagnostics)?;
            }
        }
        Item::UseDeclaration(declaration) => {
            for component in &mut declaration.path {
                substitute_identifier(
                    component,
                    &mut declaration.syntax,
                    environment,
                    diagnostics,
                )?;
            }
        }
        _ => unreachable!("unsupported item reached item substitution"),
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
        | Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_) => true,
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
        Item::Binding(_) | Item::PatternBinding(_) => true,
        Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_) => false,
        Item::UseDeclaration(_) | Item::Submodule(_) | Item::MacroDeclaration(_) => false,
    }
}

fn attach_doc(item: &mut Item, doc: String) -> bool {
    match item {
        Item::Modified(modified) => return attach_doc(&mut modified.item, doc),
        Item::VisibilitySplice(splice) => return attach_doc(&mut splice.item, doc),
        Item::Submodule(value) => value.docs.insert(0, doc),
        Item::TypeDeclaration(value) => value.docs.insert(0, doc),
        Item::MacroDeclaration(value) => value.docs.insert(0, doc),
        Item::TraitDeclaration(value) => value.docs.insert(0, doc),
        Item::Binding(value) => value.docs.insert(0, doc),
        _ => return false,
    }
    true
}

fn block_item_supported(item: &Item) -> bool {
    match item {
        Item::Binding(binding) => binding.visibility == Visibility::Private,
        Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_) => true,
        Item::Submodule(submodule) => submodule.visibility == Visibility::Private,
        Item::TypeDeclaration(declaration) => {
            declaration.visibility == Visibility::Private
                && declaration.representation_visibility == Visibility::Private
        }
        Item::UseDeclaration(declaration) => declaration.visibility == Visibility::Private,
        Item::Modified(_)
        | Item::VisibilityMacroInvocation(_)
        | Item::VisibilitySplice(_)
        | Item::RepeatedItemSplice(_)
        | Item::ExternBlock(_)
        | Item::MacroDeclaration(_)
        | Item::TraitDeclaration(_)
        | Item::TraitImplementation(_) => false,
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
        value @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => block_item_syntax(value),
    }
}

fn block_item_syntax(item: &Item) -> &Syntax {
    match item {
        Item::Binding(value) => &value.syntax,
        Item::PatternBinding(value) => &value.syntax,
        Item::Assignment(value) => &value.syntax,
        Item::Return(value) => &value.syntax,
        Item::Break(value) => &value.syntax,
        Item::Continue(value) => &value.syntax,
        Item::Expression(value) => value.syntax(),
        Item::Submodule(value) => &value.syntax,
        Item::TypeDeclaration(value) => &value.syntax,
        Item::UseDeclaration(value) => &value.syntax,
        _ => unreachable!("item-only syntax helper received a declaration item"),
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
    for bound in &mut binding.subtype_bounds {
        substitute_subtype_bound(bound, environment, diagnostics)?;
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

fn substitute_subtype_bound(
    bound: &mut crate::SubtypeBound,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    substitute_type(&mut bound.supertype, environment, diagnostics)
}

fn substitute_default_bound(
    bound: &mut crate::DefaultTypeBound,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    substitute_type(&mut bound.default, environment, diagnostics)
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
                            mutable: false,
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
            for resource in &mut function.effects.resources {
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
                substitute_identifier(namespace, &mut named.syntax, environment, diagnostics)?;
            }
            substitute_identifier(&mut named.name, &mut named.syntax, environment, diagnostics)?;
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
        Pattern::At(at) => {
            substitute_type(&mut at.binding.ty, environment, diagnostics)?;
            substitute_pattern(&mut at.pattern, environment, diagnostics)?;
        }
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
            for dependency in &mut declaration.functional_dependencies {
                substitute_identifier(
                    &mut dependency.dependent.name,
                    &mut dependency.dependent.syntax,
                    environment,
                    diagnostics,
                )?;
                for determinant in &mut dependency.determinants {
                    substitute_identifier(
                        &mut determinant.name,
                        &mut determinant.syntax,
                        environment,
                        diagnostics,
                    )?;
                }
            }
            for prerequisite in &mut declaration.prerequisites {
                substitute_trait_bound(prerequisite, environment, diagnostics)?;
            }
            for bound in &mut declaration.subtype_bounds {
                substitute_subtype_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.default_bounds {
                substitute_default_bound(bound, environment, diagnostics)?;
            }
            for member in &mut declaration.members {
                substitute_type(&mut member.annotation, environment, diagnostics)?;
                if let Some(default) = &mut member.default {
                    *default = substitute_splices(default, environment, diagnostics)?;
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            substitute_type_parameter_list(
                &mut implementation.type_parameters,
                environment,
                diagnostics,
            )?;
            for bound in &mut implementation.trait_bounds {
                substitute_trait_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut implementation.subtype_bounds {
                substitute_subtype_bound(bound, environment, diagnostics)?;
            }
            for argument in &mut implementation.arguments {
                substitute_type(argument, environment, diagnostics)?;
            }
            for member in &mut implementation.members {
                member.value = substitute_splices(&member.value, environment, diagnostics)?;
            }
        }
        item @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => {
            substitute_block_item(item, environment, diagnostics)?;
        }
        Item::TypeDeclaration(declaration) => {
            substitute_identifier(
                &mut declaration.name,
                &mut declaration.syntax,
                environment,
                diagnostics,
            )?;
            substitute_type_parameter_list(
                &mut declaration.type_parameters,
                environment,
                diagnostics,
            )?;
            for bound in &mut declaration.trait_bounds {
                substitute_trait_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.subtype_bounds {
                substitute_subtype_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.default_bounds {
                substitute_default_bound(bound, environment, diagnostics)?;
            }
            if let Some(underlying) = &mut declaration.underlying {
                substitute_type(underlying, environment, diagnostics)?;
            }
        }
        Item::Submodule(submodule) => {
            substitute_identifier(
                &mut submodule.name,
                &mut submodule.syntax,
                environment,
                diagnostics,
            )?;
            substitute_item_list(&mut submodule.module.items, environment, diagnostics)?;
        }
        Item::UseDeclaration(declaration) => {
            for component in &mut declaration.path {
                substitute_identifier(
                    component,
                    &mut declaration.syntax,
                    environment,
                    diagnostics,
                )?;
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
        Item::Binding(binding) => {
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
    syntax: &mut Syntax,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    let Some(splice) = name.strip_prefix('$') else {
        return Some(());
    };
    match environment.get(splice).map(EnvironmentBinding::get) {
        Some(Value::Syntax(SyntaxValue::Ident(identifier))) => {
            syntax.record_identifier_origin(identifier.name.clone(), &identifier.syntax);
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
        item @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => match item {
            Item::Binding(binding) => {
                if let Some(value) = &mut binding.value {
                    alpha_rename_expression(value, mark, &mut scopes);
                }
            }
            Item::PatternBinding(binding) => {
                alpha_rename_expression(&mut binding.value, mark, &mut scopes)
            }
            Item::Assignment(assignment) => {
                alpha_rename_expression(&mut assignment.target, mark, &mut scopes);
                alpha_rename_expression(&mut assignment.value, mark, &mut scopes);
            }
            Item::Return(return_) => alpha_rename_expression(&mut return_.value, mark, &mut scopes),
            Item::Break(break_) => {
                if let Some(value) = &mut break_.value {
                    alpha_rename_expression(value, mark, &mut scopes);
                }
            }
            Item::Continue(_) => {}
            Item::Expression(expression) => alpha_rename_expression(expression, mark, &mut scopes),
            Item::Submodule(submodule) => {
                for item in &mut submodule.module.items {
                    alpha_rename_item(item, mark);
                }
            }
            Item::TypeDeclaration(_) => {}
            Item::UseDeclaration(_) => {}
            _ => unreachable!("unsupported item reached block hygiene"),
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
        Pattern::At(at) => {
            at.binding
                .resolution_name
                .get_or_insert_with(|| at.binding.name.clone());
            let renamed = hygienic_name(&at.binding.name, mark);
            names.insert(at.binding.name.clone(), renamed.clone());
            at.binding.name = renamed;
            alpha_rename_pattern(&mut at.pattern, mark, names);
        }
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
        Expression::Logical(logical) => {
            alpha_rename_expression(&mut logical.left, mark, scopes);
            alpha_rename_expression(&mut logical.right, mark, scopes);
        }
        Expression::StringTemplate(template) => {
            for part in &mut template.parts {
                if let crate::StringTemplatePart::Interpolation(interpolation) = part {
                    alpha_rename_expression(&mut interpolation.expression, mark, scopes);
                }
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
    for item in &mut block.items {
        match item {
            Item::Binding(binding) => {
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
            Item::PatternBinding(binding) => {
                alpha_rename_expression(&mut binding.value, mark, scopes);
                alpha_rename_pattern(&mut binding.pattern, mark, scopes.last_mut().unwrap());
            }
            Item::Assignment(assignment) => {
                alpha_rename_expression(&mut assignment.target, mark, scopes);
                alpha_rename_expression(&mut assignment.value, mark, scopes);
            }
            Item::Return(return_) => alpha_rename_expression(&mut return_.value, mark, scopes),
            Item::Break(break_) => {
                if let Some(value) = &mut break_.value {
                    alpha_rename_expression(value, mark, scopes);
                }
            }
            Item::Continue(_) => {}
            Item::Expression(expression) => alpha_rename_expression(expression, mark, scopes),
            // A submodule's body can never reference the enclosing block's
            // local bindings, so there's nothing here for value-identifier
            // hygiene to rename.
            Item::Submodule(_) => {}
            // Likewise, a type declaration's bounds/underlying type reference
            // type parameters and other types, never value identifiers.
            Item::TypeDeclaration(_) => {}
            // A use declaration's path never references local value
            // bindings this pass tracks either.
            Item::UseDeclaration(_) => {}
            _ => unreachable!("unsupported item reached block hygiene"),
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
        Expression::Logical(value) => &mut value.syntax,
        Expression::SyntaxArgument(value) => &mut value.syntax,
        Expression::VisibilityArgument(value) => &mut value.syntax,
        Expression::Quote(value) => &mut value.syntax,
        Expression::Splice(value) => &mut value.syntax,
        Expression::Name(value) => &mut value.syntax,
        Expression::String(value) => &mut value.syntax,
        Expression::StringTemplate(value) => &mut value.syntax,
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
        Pattern::At(at) => {
            expander.freshen_syntax(&mut at.syntax, module, mark);
            expander.freshen_syntax(&mut at.binding.syntax, module, mark);
            freshen_type(expander, &mut at.binding.ty, module, mark);
            freshen_pattern(expander, &mut at.pattern, module, mark);
        }
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

fn freshen_block_item(expander: &mut MacroExpander, item: &mut Item, module: ModuleId, mark: u64) {
    match item {
        Item::Binding(binding) => freshen_binding(expander, binding, module, mark),
        Item::PatternBinding(binding) => {
            expander.freshen_syntax(&mut binding.syntax, module, mark);
            freshen_pattern(expander, &mut binding.pattern, module, mark);
            expander.freshen_expression(&mut binding.value, module, mark);
        }
        Item::Assignment(assignment) => {
            expander.freshen_syntax(&mut assignment.syntax, module, mark);
            expander.freshen_expression(&mut assignment.target, module, mark);
            expander.freshen_expression(&mut assignment.value, module, mark);
        }
        Item::Return(return_) => {
            expander.freshen_syntax(&mut return_.syntax, module, mark);
            expander.freshen_expression(&mut return_.value, module, mark);
        }
        Item::Break(break_) => {
            expander.freshen_syntax(&mut break_.syntax, module, mark);
            if let Some(value) = &mut break_.value {
                expander.freshen_expression(value, module, mark);
            }
        }
        Item::Continue(continue_) => {
            expander.freshen_syntax(&mut continue_.syntax, module, mark);
        }
        Item::Expression(expression) => expander.freshen_expression(expression, module, mark),
        Item::Submodule(submodule) => {
            expander.freshen_syntax(&mut submodule.syntax, module, mark);
            expander.freshen_syntax(&mut submodule.module.syntax, module, mark);
            for item in &mut submodule.module.items {
                freshen_item(expander, item, module, mark);
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
            for bound in &mut declaration.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.default_bounds {
                freshen_default_bound(expander, bound, module, mark);
            }
            if let Some(underlying) = &mut declaration.underlying {
                freshen_type(expander, underlying, module, mark);
            }
        }
        Item::UseDeclaration(declaration) => {
            expander.freshen_syntax(&mut declaration.syntax, module, mark);
        }
        _ => unreachable!("unsupported item reached item freshening"),
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
    for bound in &mut binding.subtype_bounds {
        freshen_subtype_bound(expander, bound, module, mark);
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
        crate::TypeParameterPattern::Effect(binding) => {
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

fn freshen_subtype_bound(
    expander: &mut MacroExpander,
    bound: &mut crate::SubtypeBound,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut bound.syntax, module, mark);
    expander.freshen_syntax(&mut bound.parameter.syntax, module, mark);
    freshen_type(expander, &mut bound.supertype, module, mark);
}

fn freshen_default_bound(
    expander: &mut MacroExpander,
    bound: &mut crate::DefaultTypeBound,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut bound.syntax, module, mark);
    expander.freshen_syntax(&mut bound.parameter.syntax, module, mark);
    freshen_type(expander, &mut bound.default, module, mark);
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
            for bound in &mut declaration.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.default_bounds {
                freshen_default_bound(expander, bound, module, mark);
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
            for dependency in &mut declaration.functional_dependencies {
                expander.freshen_syntax(&mut dependency.syntax, module, mark);
                for determinant in &mut dependency.determinants {
                    expander.freshen_syntax(&mut determinant.syntax, module, mark);
                }
                expander.freshen_syntax(&mut dependency.dependent.syntax, module, mark);
            }
            for prerequisite in &mut declaration.prerequisites {
                freshen_trait_bound(expander, prerequisite, module, mark);
            }
            for bound in &mut declaration.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.default_bounds {
                freshen_default_bound(expander, bound, module, mark);
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
            for parameter in &mut implementation.type_parameters {
                freshen_type_parameter(expander, parameter, module, mark);
            }
            for bound in &mut implementation.trait_bounds {
                freshen_trait_bound(expander, bound, module, mark);
            }
            for bound in &mut implementation.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            expander.freshen_syntax(&mut implementation.trait_name.syntax, module, mark);
            for argument in &mut implementation.arguments {
                freshen_type(expander, argument, module, mark);
            }
            for member in &mut implementation.members {
                expander.freshen_syntax(&mut member.syntax, module, mark);
                expander.freshen_expression(&mut member.value, module, mark);
            }
        }
        item @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => freshen_block_item(expander, item, module, mark),
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
            expander.freshen_syntax(&mut function.effects.syntax, module, mark);
            for resource in &mut function.effects.resources {
                freshen_type(expander, resource, module, mark);
            }
            for mutation in &mut function.mutations {
                expander.freshen_syntax(&mut mutation.syntax, module, mark);
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
