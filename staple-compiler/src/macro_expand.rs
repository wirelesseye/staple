use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

mod parse;
mod traits;

use crate::program::source_location_at;
use crate::{FloatType, ModuleId, Program, ResolvedMacro};
use parse::{ParseImpl, ParseTarget, SyntaxIntrinsic, TypeEnv};
use staple_syntax::{
    Accessor, BinaryOperator, Binding, BindingKind, BlockExpression, Diagnostic, Expression, Item,
    LogicalOperator, MacroDeclaration, ModifierArgument, ModifierInvocation, Pattern,
    SelectedImport, Span, Syntax, SyntaxId, Type, UseDeclaration, UseKind, Visibility,
    VisibilityKind, VisibilitySyntax,
};

const MAX_EXPANSION_DEPTH: usize = 128;
const MAX_EVALUATION_STEPS: usize = 1_000_000;
/// Compile-time products keep repeated elements compact. Operations that
/// materialize positions still need a bound, independent of the type-level
/// product limit.
const MAX_COMPILE_TIME_PRODUCT_ARITY: usize = 1_000_000;
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
    /// `std.source.line`, `std.source.column`, and `std.source.file`.
    SourceLocation(SourceLocationMacro),
    Doc,
    Feature,
    NoPrelude,
    TrackCaller,
    SizedOpaque,
    RecursiveConstructor,
    Quote,
    ParseQuote,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceLocationMacro {
    Line,
    Column,
    File,
}

impl SourceLocationMacro {
    fn name(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Column => "column",
            Self::File => "file",
        }
    }
}

/// A file-backed module's source, used to answer `std.source` macros.
struct SourceFile {
    /// The path reported by `file`.
    path: String,
    /// The lossless token stream of the whole file.
    tokens: Arc<[staple_syntax::SyntaxToken]>,
    /// The file text, reconstructed from `tokens` on first use.
    text: Option<String>,
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
    Parsed(Value),
    Expression(Expression),
    Visibility(VisibilitySyntax),
    Metadata(MacroCallMetadata),
    Sequence(Vec<Expression>),
}

#[derive(Clone)]
struct MacroCallMetadata {
    syntax: Syntax,
    modifiers: Vec<ModifierInvocation>,
    visibility: VisibilitySyntax,
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MetaType {
    Syntax,
    SyntaxNode,
    Expr,
    Ident(Option<String>),
    CallExpr,
    StringExpr,
    CharExpr,
    UnstructuredExpr,
    Type,
    Pattern,
    BindingPattern,
    NominalPattern,
    Item,
    Modifier,
    ModifierCall,
    UnstructuredModifier,
    ModifiedItem,
    TypeDeclarationItem,
    TypeBody,
    TypeParameter,
    TypeParameters,
    /// One `with {…}` entry of a type declaration: its resource type and
    /// whether it is `mut`. Exposed for inspection only; it has no syntax.
    ResourceDependency,
    UnstructuredItem,
    Visibility,
    MacroCallMetadata,
    Comma,
    Equals,
    FatArrow,
    /// A slotted product shape `(A; B)`: its elements are juxtaposed.
    SlottedProduct(Vec<MetaType>),
    /// A product shape `(A, B)`: a parenthesized, comma-separated group.
    Product(Vec<MetaType>),
    Optional(Box<MetaType>),
    Sequence(Box<MetaType>),
    Delimited(DelimiterKind, DelimitedMetaContents),
    /// A user type, or a standard syntax constructor such as `Separated`, as a
    /// `Parse` target. Macro signatures never produce it.
    Nominal(String, Vec<MetaType>),
}

/// The statically-known portion of a value evaluated while expanding macros.
///
/// Macro expansion precedes ordinary name resolution and type checking, so
/// compile-time programs need a small type domain of their own. `Named` is
/// deliberately retained for ordinary pure values (for example `Bool`), while
/// syntax categories use `Meta` so their structural constructors remain
/// available to match coverage.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CompileType {
    Error,
    Unknown,
    Never,
    String,
    Integer,
    Float,
    Bool,
    Named(String),
    Meta(MetaType),
    Product(Vec<CompileType>),
    /// A slotted product value `(A; B)`. Unlike a plain product, a function
    /// parameter of this type takes one juxtaposed argument per slot.
    SlottedProduct(Vec<CompileType>),
    Function(Box<CompileType>, Box<CompileType>),
    /// `Result<T, E>`, whose success is `Ok<T>` and whose failure is an `E`.
    Result(Box<CompileType>, Box<CompileType>),
    /// A `ParseStream`.
    Stream,
    /// An applied, sum, or array type kept as written, so a `Parse` target can
    /// be recovered from it.
    Written(Box<Type>),
}

#[derive(Clone)]
struct CompileBinding {
    ty: CompileType,
    mutable: bool,
}

type CompileEnvironment = HashMap<String, CompileBinding>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum CompileCoveragePattern {
    Any,
    Constructor(String, Vec<CompileCoveragePattern>),
    Literal(String),
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DelimiterKind {
    Parenthesized,
    Bracketed,
    Angled,
    Braced,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DelimitedMetaContents {
    /// Juxtaposed contents, declared as a slotted product or a single shape.
    Slotted(Vec<MetaType>),
    /// Comma-separated contents, declared as a product.
    Product(Vec<MetaType>),
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
                | Self::CharExpr
                | Self::StringExpr
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
    Ident(staple_syntax::NameExpression),
    Call(staple_syntax::CallExpression),
    Unstructured(Expression),
    Type(Type),
    Pattern(Pattern),
    Item(Box<Item>),
    Items(Vec<Item>),
    Modifier(OpaqueModifier),
    Visibility(VisibilitySyntax),
    Comma(Syntax),
    Equals(Syntax),
    FatArrow(Syntax),
    Delimited(DelimitedSyntaxValue),
    Parameters {
        kind: MetaType,
        syntax: Syntax,
        fields: Vec<(Option<String>, Value)>,
    },
}

#[derive(Clone)]
struct OpaqueModifier {
    invocation: staple_syntax::ModifierInvocation,
    /// Retained on the first modifier so destructuring and reconstructing a
    /// `ModifiedItem` preserves the original lossless wrapper syntax.
    enclosing: Option<Syntax>,
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
    Slotted(Vec<(Option<String>, Value)>),
    Product(Vec<(Option<String>, Value)>),
    Sequence(SequenceValue),
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
            Self::Raw(_) | Self::Parameters { .. } => None,
            Self::Ident(name) => Some(Expression::Name(name)),
            Self::Call(call) => Some(Expression::Call(call)),
            Self::Unstructured(expression) => Some(expression),
            Self::Delimited(delimited) => delimited.into_expression(),
            Self::Type(_)
            | Self::Pattern(_)
            | Self::Item(_)
            | Self::Items(_)
            | Self::Modifier(_)
            | Self::Visibility(_)
            | Self::Comma(_)
            | Self::Equals(_)
            | Self::FatArrow(_) => None,
        }
    }

    fn syntax(&self) -> Option<&Syntax> {
        match self {
            Self::Parameters { syntax, .. } => Some(syntax),
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
            Self::Modifier(value) => Some(&value.invocation.syntax),
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
            DelimiterKind::Bracketed | DelimiterKind::Angled => None,
            DelimiterKind::Parenthesized => {
                let values = match self.contents {
                    DelimitedValueContents::Slotted(values) => values,
                    DelimitedValueContents::Sequence(values) => {
                        values.into_iter().map(|value| (None, value)).collect()
                    }
                    DelimitedValueContents::Separated { elements, .. } => {
                        return separated_parenthesized(self.syntax, elements, &mut generated);
                    }
                    DelimitedValueContents::Product(values) => {
                        let elements = values.into_iter().map(|(_, value)| value).collect();
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
                    return Some(Expression::Product(staple_syntax::ProductExpression {
                        slotted: false,
                        syntax: self.syntax,
                        elements: Vec::new(),
                    }));
                }
                let first = expressions.remove(0);
                let expression = expressions.into_iter().fold(first, |callee, argument| {
                    Expression::Call(staple_syntax::CallExpression {
                        spread: false,
                        syntax: generated.next().unwrap_or_else(Syntax::compiler),
                        callee: Box::new(callee),
                        argument: Box::new(argument),
                    })
                });
                Some(Expression::Product(staple_syntax::ProductExpression {
                    slotted: false,
                    syntax: self.syntax,
                    elements: vec![staple_syntax::ProductElement {
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
                    DelimitedValueContents::Slotted(values) => values
                        .into_iter()
                        .map(|(_, value)| value)
                        .collect::<Vec<_>>(),
                    DelimitedValueContents::Sequence(values) => values.to_vec(),
                    DelimitedValueContents::Separated { .. }
                    | DelimitedValueContents::Product(_) => {
                        return None;
                    }
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
        SyntaxValue::Parameters { .. } => "type parameter syntax",
        SyntaxValue::Ident(_) | SyntaxValue::Call(_) | SyntaxValue::Unstructured(_) => "expression",
        SyntaxValue::Type(_) => "type",
        SyntaxValue::Pattern(_) => "pattern",
        SyntaxValue::Item(_) => "item",
        SyntaxValue::Items(_) => "item sequence",
        SyntaxValue::Modifier(_) => "modifier",
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
            DelimiterKind::Angled => "angle-bracketed",
            DelimiterKind::Braced => "braced",
        },
    }
}

#[derive(Clone)]
enum Value {
    Syntax(SyntaxValue),
    Function {
        module: ModuleId,
        function: staple_syntax::FunctionExpression,
        environment: Environment,
        quote_result: Option<MetaType>,
        result_target: Option<MetaType>,
        /// The `Parse` implementation parameters in scope where the function
        /// was written, so closures and default members keep their bindings.
        type_env: TypeEnv,
    },
    Helper(ModuleId, Binding),
    /// Trait identity, member name, explicit types, and supplied curried slots.
    TraitMember(traits::TraitKey, String, Vec<Type>, Vec<Value>),
    Method(ModuleId, String),
    TypeApplied(Box<Value>, Vec<Type>),
    /// A compiler-implemented `std.syntax` function.
    Intrinsic(SyntaxIntrinsic),
    /// A copyable cursor over a syntax fragment: the stream's tokens and the
    /// index, within them, of the first unread one.
    Stream {
        syntax: Syntax,
        cursor: usize,
    },
    Product(ProductValue, bool),
    Integer(i128),
    Char(char),
    /// A float and the float type it is known to have. `None` is an
    /// unannotated literal, or arithmetic over only those, whose type the
    /// checker infers from context later.
    Float(f64, Option<FloatType>),
    String(String),
    Nominal(String, Box<Value>),
    Sequence(SequenceValue),
    Separated {
        elements: Vec<Value>,
        separator: Box<Value>,
        trailing: bool,
    },
}

/// Compile-time product storage. A repeated value keeps its element once.
#[derive(Clone)]
enum ProductValue {
    Elements(Vec<(Option<String>, Value)>),
    Repeated {
        element: Box<(Option<String>, Value)>,
        count: usize,
    },
}

#[allow(dead_code)]
impl ProductValue {
    fn repeated(value: Value, count: usize) -> Self {
        Self::Repeated {
            element: Box::new((None, value)),
            count,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Elements(elements) => elements.len(),
            Self::Repeated { count, .. } => *count,
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn iter(&self) -> ProductValueIter<'_> {
        ProductValueIter {
            product: self,
            front: 0,
            back: self.len(),
        }
    }

    fn get(&self, index: usize) -> Option<&(Option<String>, Value)> {
        match self {
            Self::Elements(elements) => elements.get(index),
            Self::Repeated { element, count } if index < *count => Some(element.as_ref()),
            Self::Repeated { .. } => None,
        }
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut (Option<String>, Value)> {
        self.materialize().get_mut(index)
    }

    fn first(&self) -> Option<&(Option<String>, Value)> {
        self.get(0)
    }

    fn last(&self) -> Option<&(Option<String>, Value)> {
        self.len().checked_sub(1).and_then(|index| self.get(index))
    }

    fn iter_mut(&mut self) -> std::slice::IterMut<'_, (Option<String>, Value)> {
        self.materialize().iter_mut()
    }

    fn materialize(&mut self) -> &mut Vec<(Option<String>, Value)> {
        if let Self::Repeated { element, count } = self {
            let elements = vec![element.as_ref().clone(); *count];
            *self = Self::Elements(elements);
        }
        match self {
            Self::Elements(elements) => elements,
            Self::Repeated { .. } => unreachable!("materialized above"),
        }
    }

    fn into_vec(self) -> Vec<(Option<String>, Value)> {
        match self {
            Self::Elements(elements) => elements,
            Self::Repeated { element, count } => {
                vec![element.as_ref().clone(); count]
            }
        }
    }

    fn push(&mut self, element: (Option<String>, Value)) {
        self.materialize().push(element);
    }

    fn extend(&mut self, elements: impl IntoIterator<Item = (Option<String>, Value)>) {
        self.materialize().extend(elements);
    }

    fn truncate(&mut self, len: usize) {
        self.materialize().truncate(len);
    }

    fn remove(&mut self, index: usize) -> (Option<String>, Value) {
        self.materialize().remove(index)
    }

    fn pop(&mut self) -> Option<(Option<String>, Value)> {
        self.materialize().pop()
    }

    fn clear(&mut self) {
        *self = Self::Elements(Vec::new());
    }

    fn to_vec(&self) -> Vec<(Option<String>, Value)> {
        self.iter().cloned().collect()
    }
}

impl From<Vec<(Option<String>, Value)>> for ProductValue {
    fn from(elements: Vec<(Option<String>, Value)>) -> Self {
        Self::Elements(elements)
    }
}

impl FromIterator<(Option<String>, Value)> for ProductValue {
    fn from_iter<T: IntoIterator<Item = (Option<String>, Value)>>(iter: T) -> Self {
        Self::Elements(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a ProductValue {
    type Item = &'a (Option<String>, Value);
    type IntoIter = ProductValueIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for ProductValue {
    type Item = (Option<String>, Value);
    type IntoIter = std::vec::IntoIter<(Option<String>, Value)>;
    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl std::ops::Index<usize> for ProductValue {
    type Output = (Option<String>, Value);
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("product index in bounds")
    }
}

struct ProductValueIter<'a> {
    product: &'a ProductValue,
    front: usize,
    back: usize,
}

impl<'a> Iterator for ProductValueIter<'a> {
    type Item = &'a (Option<String>, Value);
    fn next(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        let index = self.front;
        self.front += 1;
        self.product.get(index)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back - self.front;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ProductValueIter<'_> {}
impl DoubleEndedIterator for ProductValueIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        self.back -= 1;
        self.product.get(self.back)
    }
}

/// Shared storage plus a cursor makes consuming a cons tail constant time.
#[derive(Clone, Default)]
struct SequenceValue {
    values: Rc<Vec<Value>>,
    start: usize,
}
impl From<Vec<Value>> for SequenceValue {
    fn from(values: Vec<Value>) -> Self {
        Self {
            values: Rc::new(values),
            start: 0,
        }
    }
}
impl FromIterator<Value> for SequenceValue {
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        Vec::from_iter(iter).into()
    }
}
impl std::ops::Deref for SequenceValue {
    type Target = [Value];
    fn deref(&self) -> &[Value] {
        &self.values[self.start..]
    }
}
impl IntoIterator for SequenceValue {
    type Item = Value;
    type IntoIter = std::vec::IntoIter<Value>;
    fn into_iter(self) -> Self::IntoIter {
        match Rc::try_unwrap(self.values) {
            Ok(mut values) => {
                values.drain(..self.start);
                values.into_iter()
            }
            Err(values) => values[self.start..].to_vec().into_iter(),
        }
    }
}
impl<'a> IntoIterator for &'a SequenceValue {
    type Item = &'a Value;
    type IntoIter = std::slice::Iter<'a, Value>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl SequenceValue {
    fn tail(&self) -> Self {
        Self {
            values: self.values.clone(),
            start: self.start + 1,
        }
    }
    fn storage(&mut self) -> &mut Vec<Value> {
        if self.start != 0 {
            *self = self.to_vec().into();
        }
        Rc::make_mut(&mut self.values)
    }
    fn push(&mut self, value: Value) {
        self.storage().push(value);
    }
    fn insert(&mut self, index: usize, value: Value) {
        self.storage().insert(index, value);
    }
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
            Value::Syntax(value) => Some(staple_syntax::ProductElement {
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
    Some(Expression::Product(staple_syntax::ProductExpression {
        slotted: false,
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
        pair[0].kind == staple_syntax::TokenKind::Dollar
            && pair[1].kind == staple_syntax::TokenKind::Identifier
            && environment
                .get(&pair[1].text)
                .map(EnvironmentBinding::get)
                .is_some_and(|value| {
                    matches!(
                        value,
                        Value::Syntax(SyntaxValue::Raw(_) | SyntaxValue::Parameters { .. })
                            | Value::Syntax(SyntaxValue::Delimited(DelimitedSyntaxValue {
                                kind: DelimiterKind::Angled,
                                ..
                            }))
                    )
                })
    })
}

fn append_syntax_tokens(output: &mut Vec<staple_syntax::SyntaxToken>, syntax: &Syntax) {
    let mut occurrences = HashMap::<String, usize>::new();
    for token in syntax.tokens() {
        let mut token = token.clone();
        if token.origin.is_none() && !token.kind.is_trivia() {
            let occurrence = occurrences.entry(token.text.clone()).or_default();
            token.origin = syntax
                .identifier_origins
                .iter()
                .filter(|(name, _)| name == &token.text)
                .nth(*occurrence)
                .map(|(_, origin)| origin.clone())
                .or_else(|| match &syntax.span {
                    Span::User { source, .. } => Some(Span::User {
                        source: source.clone(),
                        range: token.span.clone(),
                        location: None,
                    }),
                    Span::Compiler => None,
                });
            *occurrence += 1;
        }
        output.push(token);
    }
}

/// Collects the source of every file-backed module, keyed by the source name
/// its spans carry, for the `std.source` macros. `file` reports a path relative
/// to the containing package's directory (or the module root when there is no
/// package) when the file lies inside it.
fn source_files(program: &Program) -> HashMap<Arc<str>, SourceFile> {
    let mut files = HashMap::new();
    for module in program.modules() {
        let syntax = &module.syntax.syntax;
        let Span::User {
            source: Some(name), ..
        } = &syntax.span
        else {
            continue;
        };
        files.entry(name.clone()).or_insert_with(|| SourceFile {
            path: program.module_source_path(module.id),
            tokens: syntax.tokens.clone(),
            text: None,
        });
    }
    files
}

/// Whether `inner` lies within `outer` in the same source.
fn span_contains(outer: &Span, inner: &Span) -> bool {
    match (outer, inner) {
        (
            Span::User {
                source: Some(outer_source),
                range: outer_range,
                ..
            },
            Span::User {
                source: Some(inner_source),
                range: inner_range,
                ..
            },
        ) => {
            outer_source == inner_source
                && outer_range.start <= inner_range.start
                && inner_range.end <= outer_range.end
        }
        _ => false,
    }
}

/// Which kind of declaration member a modifier is written on.
#[derive(Clone, Copy)]
enum MemberKind {
    TraitFunction,
    TraitType,
    Implementation,
    Other,
}

pub(crate) struct MacroAnalysis {
    pub definitions: HashMap<SyntaxId, ResolvedMacro>,
    pub invocations: HashMap<SyntaxId, ResolvedMacro>,
    pub helpers: Vec<(ModuleId, Binding)>,
    pub binding_types: HashMap<SyntaxId, String>,
    /// Functions that exist only during macro expansion (the `std.syntax`
    /// intrinsics), so importing one is not an error.
    pub compile_time_items: HashSet<(ModuleId, String)>,
    /// Macro-generated references mapped to the span of the outermost macro
    /// invocation that produced them.
    pub caller_location_spans: HashMap<SyntaxId, Span>,
    pub next_syntax_id: usize,
}

/// The standard-library modules operator syntax resolves against, independent
/// of the imports and names in the calling module.
#[derive(Clone, Copy, Default)]
pub(crate) struct StdModules {
    /// `std.ops`, defining the operator traits.
    pub ops: Option<ModuleId>,
    /// `std.try`, defining `Try`.
    pub try_: Option<ModuleId>,
    /// `std.result`, defining `Ok`.
    pub result: Option<ModuleId>,
    /// `std.cmp`, defining `Eq` and `PartialOrd`.
    pub cmp: Option<ModuleId>,
    /// `std.convert`, defining `Convert`.
    pub convert: Option<ModuleId>,
    /// `std.range`, defining `range` and `range_inclusive`.
    pub range: Option<ModuleId>,
    /// `std.boolean`, defining `Bool`.
    pub boolean: Option<ModuleId>,
}

fn freshened_syntax(syntax: &Syntax, next_syntax_id: &mut usize) -> Syntax {
    let mut syntax = syntax.clone();
    syntax.id = SyntaxId(*next_syntax_id);
    *next_syntax_id += 1;
    syntax
}

fn lower_binary_expression(
    binary: staple_syntax::BinaryExpression,
    next_syntax_id: &mut usize,
    ops_module: StdModules,
) -> Expression {
    let staple_syntax::BinaryExpression {
        syntax,
        operator_syntax,
        operator,
        left,
        right,
    } = binary;
    if matches!(operator, BinaryOperator::And | BinaryOperator::Or) {
        let bool_type = Type::Named(staple_syntax::NamedType {
            syntax: {
                let mut syntax = freshened_syntax(&operator_syntax, next_syntax_id);
                syntax.definition_module = ops_module.boolean.map(|module| module.0);
                syntax
            },
            namespace: None,
            name: "Bool".to_owned(),
        });
        return Expression::Logical(staple_syntax::LogicalExpression {
            syntax,
            operator: if operator == BinaryOperator::And {
                LogicalOperator::And
            } else {
                LogicalOperator::Or
            },
            left,
            right,
            bool_type,
        });
    }
    if matches!(
        operator,
        BinaryOperator::Range | BinaryOperator::RangeInclusive
    ) {
        let name = Expression::Name(staple_syntax::NameExpression {
            syntax: {
                let mut syntax = freshened_syntax(&operator_syntax, next_syntax_id);
                syntax.definition_module = ops_module.range.map(|module| module.0);
                syntax
            },
            name: if operator == BinaryOperator::Range {
                "range"
            } else {
                "range_inclusive"
            }
            .to_owned(),
        });
        let inner = Expression::Call(staple_syntax::CallExpression {
            spread: false,
            syntax: freshened_syntax(&syntax, next_syntax_id),
            callee: Box::new(name),
            argument: left,
        });
        return Expression::Call(staple_syntax::CallExpression {
            spread: false,
            syntax,
            callee: Box::new(inner),
            argument: right,
        });
    }
    let (trait_name, method_name) = match operator {
        BinaryOperator::Add => ("Add", "add"),
        BinaryOperator::Subtract => ("Subtract", "subtract"),
        BinaryOperator::Multiply => ("Multiply", "multiply"),
        BinaryOperator::Divide => ("Divide", "divide"),
        BinaryOperator::Remainder => ("Remainder", "remainder"),
        BinaryOperator::Equal => ("Eq", "eq"),
        BinaryOperator::NotEqual => ("Eq", "ne"),
        BinaryOperator::Less => ("PartialOrd", "lt"),
        BinaryOperator::LessEqual => ("PartialOrd", "le"),
        BinaryOperator::Greater => ("PartialOrd", "gt"),
        BinaryOperator::GreaterEqual => ("PartialOrd", "ge"),
        BinaryOperator::Range
        | BinaryOperator::RangeInclusive
        | BinaryOperator::And
        | BinaryOperator::Or => unreachable!(),
    };
    let name = Expression::Name(staple_syntax::NameExpression {
        syntax: {
            let mut syntax = freshened_syntax(&operator_syntax, next_syntax_id);
            // Operator syntax resolves at the standard trait definition,
            // independently of imports and names in the calling module.
            let module = if matches!(trait_name, "Eq" | "PartialOrd") {
                ops_module.cmp
            } else {
                ops_module.ops
            };
            syntax.definition_module = module.map(|module| module.0);
            syntax
        },
        name: trait_name.to_owned(),
    });
    let access = Expression::Access(staple_syntax::AccessExpression {
        syntax: freshened_syntax(&operator_syntax, next_syntax_id),
        value: Box::new(name),
        accessor: Accessor::Name(method_name.to_owned()),
    });
    let element = |value, next_syntax_id: &mut usize| staple_syntax::ProductElement {
        syntax: freshened_syntax(&syntax, next_syntax_id),
        name: None,
        designated: false,
        value,
        spread: false,
        named_spread: false,
    };
    let argument = Expression::Product(staple_syntax::ProductExpression {
        // Every binary operator trait declares a slotted parameter, so the
        // operands fill separate slots.
        slotted: true,
        syntax: freshened_syntax(&syntax, next_syntax_id),
        elements: vec![
            element(*left, next_syntax_id),
            element(*right, next_syntax_id),
        ],
    });
    Expression::Call(staple_syntax::CallExpression {
        spread: false,
        syntax,
        callee: Box::new(access),
        argument: Box::new(argument),
    })
}

fn lower_unary_expression(
    unary: staple_syntax::UnaryExpression,
    next_syntax_id: &mut usize,
    ops_module: StdModules,
) -> Expression {
    let staple_syntax::UnaryExpression {
        syntax,
        operator_syntax,
        operator,
        operand,
    } = unary;
    let (trait_name, method_name) = operator.trait_method();
    let name = Expression::Name(staple_syntax::NameExpression {
        syntax: {
            let mut syntax = freshened_syntax(&operator_syntax, next_syntax_id);
            syntax.definition_module = ops_module.ops.map(|module| module.0);
            syntax
        },
        name: trait_name.to_owned(),
    });
    let access = Expression::Access(staple_syntax::AccessExpression {
        syntax: freshened_syntax(&operator_syntax, next_syntax_id),
        value: Box::new(name),
        accessor: Accessor::Name(method_name.to_owned()),
    });
    Expression::Call(staple_syntax::CallExpression {
        spread: false,
        syntax,
        callee: Box::new(access),
        argument: operand,
    })
}

/// Rewrites `operand ?` into
/// `match (Try.branch operand) { Ok output => output, residual => return residual }`,
/// resolving `Try` and `Ok` at their standard-library definitions. The arm
/// binders use names no source can spell, so nothing in scope can capture them.
fn lower_try_expression(
    try_: staple_syntax::TryExpression,
    next_syntax_id: &mut usize,
    std_modules: StdModules,
) -> Expression {
    let staple_syntax::TryExpression {
        syntax,
        operator_syntax,
        operand,
    } = try_;
    let defined_in = |module: Option<ModuleId>, next_syntax_id: &mut usize| {
        let mut syntax = freshened_syntax(&operator_syntax, next_syntax_id);
        syntax.definition_module = module.map(|module| module.0);
        syntax
    };
    let name = |text: &str, next_syntax_id: &mut usize| {
        Expression::Name(staple_syntax::NameExpression {
            syntax: freshened_syntax(&operator_syntax, next_syntax_id),
            name: text.to_owned(),
        })
    };
    let binding = |text: &str, next_syntax_id: &mut usize| {
        Pattern::Binding(staple_syntax::BindingPattern {
            syntax: freshened_syntax(&operator_syntax, next_syntax_id),
            mutable: false,
            moved: false,
            name: text.to_owned(),
            resolution_name: None,
            ty: Type::Inferred(staple_syntax::InferredType {
                syntax: freshened_syntax(&operator_syntax, next_syntax_id),
            }),
        })
    };
    let branch = Expression::Access(staple_syntax::AccessExpression {
        syntax: freshened_syntax(&operator_syntax, next_syntax_id),
        value: Box::new(Expression::Name(staple_syntax::NameExpression {
            syntax: defined_in(std_modules.try_, next_syntax_id),
            name: "Try".to_owned(),
        })),
        accessor: Accessor::Name("branch".to_owned()),
    });
    let subject = Expression::Call(staple_syntax::CallExpression {
        spread: false,
        syntax: freshened_syntax(&syntax, next_syntax_id),
        callee: Box::new(branch),
        argument: operand,
    });
    // Use the same residual-aware path as `let ?Ok output = value`.
    // A catch-all match binding retains the entire source sum and would
    // incorrectly require the success type to match the enclosing result.
    let pattern = Pattern::Nominal(staple_syntax::NominalPattern {
        syntax: defined_in(std_modules.result, next_syntax_id),
        namespace: None,
        name: "Ok".to_owned(),
        moved: false,
        argument: Box::new(binding("$output", next_syntax_id)),
        annotation: None,
    });
    Expression::Block(staple_syntax::BlockExpression {
        syntax: syntax.clone(),
        items: vec![
            Item::PatternBinding(staple_syntax::PatternBinding {
                syntax: freshened_syntax(&syntax, next_syntax_id),
                kind: staple_syntax::PatternBindingKind::Propagating,
                pattern,
                value: subject,
            }),
            Item::Expression(name("$output", next_syntax_id)),
        ],
    })
}

/// Rewrites `value as Type` into `Convert.convert.<_, Type> value`, resolving
/// `Convert` at its standard-library definition.
fn lower_conversion_expression(
    conversion: staple_syntax::ConversionExpression,
    next_syntax_id: &mut usize,
    std_modules: StdModules,
) -> Expression {
    let staple_syntax::ConversionExpression {
        syntax,
        operator_syntax,
        value,
        ty,
    } = conversion;
    let name = Expression::Name(staple_syntax::NameExpression {
        syntax: {
            let mut syntax = freshened_syntax(&operator_syntax, next_syntax_id);
            syntax.definition_module = std_modules.convert.map(|module| module.0);
            syntax
        },
        name: "Convert".to_owned(),
    });
    let access = Expression::Access(staple_syntax::AccessExpression {
        syntax: freshened_syntax(&operator_syntax, next_syntax_id),
        value: Box::new(name),
        accessor: Accessor::Name("convert".to_owned()),
    });
    let source = Type::Inferred(staple_syntax::InferredType {
        syntax: freshened_syntax(&operator_syntax, next_syntax_id),
    });
    let callee = Expression::TypeApplication(Box::new(staple_syntax::TypeApplicationExpression {
        syntax: freshened_syntax(&operator_syntax, next_syntax_id),
        value: Box::new(access),
        arguments: vec![source, ty],
    }));
    Expression::Call(staple_syntax::CallExpression {
        spread: false,
        syntax,
        callee: Box::new(callee),
        argument: value,
    })
}

/// Rewrites `receiver |> stage` into nested calls of the stage's method with
/// the receiver as the first argument: `value |> method a b` becomes
/// `((().method value) a) b`, whose method head carries `Syntax::pipe_stage` so
/// checking selects a companion or trait method for the receiver's type. The
/// selector's empty value carries no receiver copy, so the receiver occurs
/// once and keeps its syntax identity and single evaluation.
fn lower_pipe_expression(
    pipe: staple_syntax::PipeExpression,
    next_syntax_id: &mut usize,
) -> Expression {
    fn ungroup(expression: Expression) -> Expression {
        match expression {
            Expression::Product(mut product) if product.is_grouping() => {
                ungroup(product.elements.remove(0).value)
            }
            expression => expression,
        }
    }
    fn split(stage: Expression, arguments: &mut Vec<(Expression, bool)>) -> Expression {
        match ungroup(stage) {
            Expression::Call(call) => {
                let head = split(*call.callee, arguments);
                arguments.push((*call.argument, call.spread));
                head
            }
            head => head,
        }
    }
    fn select(head: Expression, empty: Expression) -> Expression {
        match head {
            Expression::Name(name) => {
                let mut syntax = name.syntax;
                syntax.pipe_stage = true;
                Expression::Access(staple_syntax::AccessExpression {
                    syntax,
                    value: Box::new(empty),
                    accessor: Accessor::Method(name.name),
                })
            }
            Expression::Access(mut access) => {
                access.syntax.pipe_stage = true;
                Expression::Access(access)
            }
            Expression::TypeApplication(mut application) => {
                application.value = Box::new(select(*application.value, empty));
                application.syntax.pipe_stage = true;
                Expression::TypeApplication(application)
            }
            // The parser only accepts method stages.
            head => head,
        }
    }
    let staple_syntax::PipeExpression {
        syntax,
        receiver,
        stage,
        ..
    } = pipe;
    let mut arguments = Vec::new();
    let head = split(*stage, &mut arguments);
    let empty = Expression::Product(staple_syntax::ProductExpression {
        slotted: false,
        syntax: Syntax::synthetic(SyntaxId(*next_syntax_id), head.syntax().span.clone()),
        elements: Vec::new(),
    });
    *next_syntax_id += 1;
    let head = select(head, empty);
    let mut result = Expression::Call(staple_syntax::CallExpression {
        spread: false,
        syntax: freshened_syntax(&syntax, next_syntax_id).ending_at(head.syntax()),
        callee: Box::new(head),
        argument: receiver,
    });
    for (argument, spread) in arguments {
        result = Expression::Call(staple_syntax::CallExpression {
            spread,
            syntax: freshened_syntax(&syntax, next_syntax_id).ending_at(argument.syntax()),
            callee: Box::new(result),
            argument: Box::new(argument),
        });
    }
    if let Expression::Call(call) = &mut result {
        call.syntax = syntax;
    }
    result
}

/// Moves the annotation of a parenthesized nominal pattern, `(Box value): T`,
/// onto the nominal pattern it annotates.
fn desugar_pattern(pattern: &mut Pattern) {
    match pattern {
        Pattern::Product(product) if product.annotation.is_some() => {
            let moved = product.moved;
            let annotation = product.annotation.take();
            let Some(Pattern::Nominal(mut nominal)) = product.elements.pop() else {
                unreachable!("the parser only annotates a single nominal pattern")
            };
            nominal.moved |= moved;
            nominal.annotation = annotation;
            *pattern = Pattern::Nominal(nominal);
            desugar_pattern(pattern);
        }
        Pattern::Product(product) => {
            for element in &mut product.elements {
                desugar_pattern(element);
            }
        }
        Pattern::Nominal(nominal) => desugar_pattern(&mut nominal.argument),
        Pattern::At(at) => desugar_pattern(&mut at.pattern),
        Pattern::Binding(_)
        | Pattern::Wildcard(_)
        | Pattern::StringLiteral(_)
        | Pattern::Splice(_) => {}
    }
}

pub(crate) fn desugar_program(
    program: &mut Program,
    next_syntax_id: &mut usize,
    ops_module: StdModules,
) {
    for module in program.modules_mut() {
        for item in &mut module.syntax.items {
            desugar_item(item, next_syntax_id, ops_module);
        }
    }
}

pub(crate) fn desugar_macro_analysis(
    analysis: &mut MacroAnalysis,
    next_syntax_id: &mut usize,
    ops_module: StdModules,
) {
    for (_, binding) in &mut analysis.helpers {
        if let Some(value) = &mut binding.value {
            desugar_expression(value, next_syntax_id, ops_module);
        }
    }
}

fn desugar_item(item: &mut Item, next_syntax_id: &mut usize, ops_module: StdModules) {
    match item {
        Item::Modified(modified) => {
            for modifier in &mut modified.modifiers {
                if let Some(expression) = modifier
                    .argument
                    .as_mut()
                    .and_then(|argument| argument.expression.as_mut())
                {
                    desugar_expression(expression, next_syntax_id, ops_module);
                }
            }
            desugar_item(&mut modified.item, next_syntax_id, ops_module);
        }
        Item::VisibilityMacroInvocation(invocation) => {
            for modifier in &mut invocation.modifiers {
                if let Some(expression) = modifier
                    .argument
                    .as_mut()
                    .and_then(|argument| argument.expression.as_mut())
                {
                    desugar_expression(expression, next_syntax_id, ops_module);
                }
            }
            desugar_expression(&mut invocation.expression, next_syntax_id, ops_module);
        }
        Item::VisibilitySplice(splice) => {
            desugar_item(&mut splice.item, next_syntax_id, ops_module)
        }
        Item::ExternBlock(block) => {
            for binding in &mut block.bindings {
                if let Some(value) = &mut binding.value {
                    desugar_expression(value, next_syntax_id, ops_module);
                }
            }
        }
        Item::TraitDeclaration(declaration) => {
            // Each associated type is a hidden trailing trait parameter
            // determined by the explicit parameters; its `where` constraints
            // become the trait's. The hidden parameter shares the associated
            // type's syntax (see `TraitDeclaration::explicit_type_parameters`).
            for associated in &mut declaration.associated_types {
                declaration
                    .type_parameters
                    .push(staple_syntax::TypeParameterPattern::Binding(
                        staple_syntax::TypeParameterBinding {
                            syntax: associated.syntax.clone(),
                            name: associated.name.clone(),
                        },
                    ));
                declaration
                    .prerequisites
                    .append(&mut associated.trait_bounds);
                declaration
                    .subtype_bounds
                    .append(&mut associated.subtype_bounds);
                declaration
                    .equality_bounds
                    .append(&mut associated.equality_bounds);
                declaration
                    .sized_relaxations
                    .append(&mut associated.sized_relaxations);
            }
            for member in &mut declaration.members {
                if let Some(default) = &mut member.default {
                    desugar_expression(default, next_syntax_id, ops_module);
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            for member in &mut implementation.members {
                desugar_expression(&mut member.value, next_syntax_id, ops_module);
            }
        }
        Item::MacroDeclaration(declaration) => {
            if let Some(value) = &mut declaration.value {
                desugar_expression(value, next_syntax_id, ops_module);
            }
        }
        Item::Binding(binding) => {
            if let Some(value) = &mut binding.value {
                desugar_expression(value, next_syntax_id, ops_module);
            }
        }
        Item::PatternBinding(binding) => {
            desugar_pattern(&mut binding.pattern);
            desugar_expression(&mut binding.value, next_syntax_id, ops_module)
        }
        Item::Assignment(assignment) => {
            desugar_expression(&mut assignment.target, next_syntax_id, ops_module);
            desugar_expression(&mut assignment.value, next_syntax_id, ops_module);
        }
        Item::Return(return_) => desugar_expression(&mut return_.value, next_syntax_id, ops_module),
        Item::Break(break_) => {
            if let Some(value) = &mut break_.value {
                desugar_expression(value, next_syntax_id, ops_module);
            }
        }
        Item::Expression(expression) => desugar_expression(expression, next_syntax_id, ops_module),
        Item::Submodule(submodule) => {
            for item in &mut submodule.module.items {
                desugar_item(item, next_syntax_id, ops_module);
            }
        }
        Item::RepeatedItemSplice(_)
        | Item::Continue(_)
        | Item::TypeDeclaration(_)
        | Item::UseDeclaration(_) => {}
    }
}

fn desugar_expression(
    expression: &mut Expression,
    next_syntax_id: &mut usize,
    ops_module: StdModules,
) {
    match expression {
        Expression::Binary(_) => {
            let Expression::Binary(mut binary) = std::mem::replace(
                expression,
                Expression::Name(staple_syntax::NameExpression {
                    syntax: Syntax::compiler(),
                    name: String::new(),
                }),
            ) else {
                unreachable!()
            };
            desugar_expression(&mut binary.left, next_syntax_id, ops_module);
            desugar_expression(&mut binary.right, next_syntax_id, ops_module);
            *expression = lower_binary_expression(binary, next_syntax_id, ops_module);
        }
        Expression::Unary(_) => {
            let Expression::Unary(mut unary) = std::mem::replace(
                expression,
                Expression::Name(staple_syntax::NameExpression {
                    syntax: Syntax::compiler(),
                    name: String::new(),
                }),
            ) else {
                unreachable!()
            };
            desugar_expression(&mut unary.operand, next_syntax_id, ops_module);
            *expression = lower_unary_expression(unary, next_syntax_id, ops_module);
        }
        Expression::Pipe(_) => {
            let Expression::Pipe(mut pipe) = std::mem::replace(
                expression,
                Expression::Name(staple_syntax::NameExpression {
                    syntax: Syntax::compiler(),
                    name: String::new(),
                }),
            ) else {
                unreachable!()
            };
            desugar_expression(&mut pipe.receiver, next_syntax_id, ops_module);
            desugar_expression(&mut pipe.stage, next_syntax_id, ops_module);
            *expression = lower_pipe_expression(pipe, next_syntax_id);
        }
        Expression::Function(function) => {
            desugar_pattern(&mut function.pattern);
            // A single-slot parameter `(a;) => body` takes the same argument
            // as `a => body`.
            if let Pattern::Product(product) = &mut function.pattern
                && product.slotted
                && product.elements.len() == 1
                && !product.mutable
                && !product.moved
            {
                function.pattern = product.elements.remove(0);
            }
            desugar_expression(&mut function.body, next_syntax_id, ops_module)
        }
        Expression::TypeApplication(application) => {
            desugar_expression(&mut application.value, next_syntax_id, ops_module)
        }
        Expression::TypeAscription(ascription) => {
            desugar_expression(&mut ascription.value, next_syntax_id, ops_module)
        }
        Expression::Match(match_) => {
            desugar_expression(&mut match_.subject, next_syntax_id, ops_module);
            for arm in &mut match_.arms {
                desugar_pattern(&mut arm.pattern);
                desugar_expression(&mut arm.body, next_syntax_id, ops_module);
            }
        }
        Expression::Loop(loop_) => {
            for item in &mut loop_.body.items {
                desugar_item(item, next_syntax_id, ops_module);
            }
        }
        Expression::Coro(coro) => {
            for item in &mut coro.body.items {
                desugar_item(item, next_syntax_id, ops_module);
            }
        }
        Expression::Await(await_) => {
            desugar_expression(&mut await_.operand, next_syntax_id, ops_module)
        }
        Expression::Try(_) => {
            let Expression::Try(mut try_) = std::mem::replace(
                expression,
                Expression::Name(staple_syntax::NameExpression {
                    syntax: Syntax::compiler(),
                    name: String::new(),
                }),
            ) else {
                unreachable!()
            };
            desugar_expression(&mut try_.operand, next_syntax_id, ops_module);
            *expression = lower_try_expression(try_, next_syntax_id, ops_module);
        }
        Expression::Conversion(_) => {
            let Expression::Conversion(mut conversion) = std::mem::replace(
                expression,
                Expression::Name(staple_syntax::NameExpression {
                    syntax: Syntax::compiler(),
                    name: String::new(),
                }),
            ) else {
                unreachable!()
            };
            desugar_expression(&mut conversion.value, next_syntax_id, ops_module);
            *expression = lower_conversion_expression(*conversion, next_syntax_id, ops_module);
        }
        Expression::With(with) => {
            desugar_expression(&mut with.value, next_syntax_id, ops_module);
            for item in &mut with.body.items {
                desugar_item(item, next_syntax_id, ops_module);
            }
        }
        Expression::Block(block) => {
            for item in &mut block.items {
                desugar_item(item, next_syntax_id, ops_module);
            }
        }
        Expression::Product(product) => {
            for element in &mut product.elements {
                desugar_expression(&mut element.value, next_syntax_id, ops_module);
            }
        }
        Expression::RepeatedProduct(repeated) => {
            desugar_expression(&mut repeated.value, next_syntax_id, ops_module);
        }
        Expression::Call(call) => {
            desugar_expression(&mut call.callee, next_syntax_id, ops_module);
            desugar_expression(&mut call.argument, next_syntax_id, ops_module);
        }
        Expression::Access(access) => {
            desugar_expression(&mut access.value, next_syntax_id, ops_module)
        }
        Expression::Index(index) => {
            desugar_expression(&mut index.value, next_syntax_id, ops_module);
            desugar_expression(&mut index.index, next_syntax_id, ops_module);
        }
        Expression::Logical(logical) => {
            desugar_expression(&mut logical.left, next_syntax_id, ops_module);
            desugar_expression(&mut logical.right, next_syntax_id, ops_module);
        }
        Expression::StringTemplate(template) => {
            for part in &mut template.parts {
                if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                    desugar_expression(&mut interpolation.expression, next_syntax_id, ops_module);
                }
            }
        }
        Expression::Quote(quote) => match &mut quote.template {
            staple_syntax::QuoteTemplate::Expression(expression) => {
                desugar_expression(expression, next_syntax_id, ops_module)
            }
            staple_syntax::QuoteTemplate::Item(item) => {
                desugar_item(item, next_syntax_id, ops_module)
            }
            staple_syntax::QuoteTemplate::Items(items) => {
                for item in items {
                    desugar_item(item, next_syntax_id, ops_module);
                }
            }
            staple_syntax::QuoteTemplate::Raw => {}
        },
        Expression::Resource(_)
        | Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Splice(_)
        | Expression::Name(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Char(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
    }
}

/// Expand every macro invocation in `program`, returning the program with its
/// syntax trees rewritten in place. This is the same expansion pass name
/// resolution runs, exposed on its own for tooling that wants to inspect the
/// result (for example `staple expand`).
///
/// Runs on a dedicated thread with a large stack for the same reason
/// [`crate::NameResolver::resolve_program`] does: expansion is a deep recursive
/// walk over the whole program, standard library included, and helper
/// evaluation adds further native-stack nesting.
pub fn expand_macros(program: Program) -> Result<Program, Vec<Diagnostic>> {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || expand_program(program).map(|(program, _)| program))
        .expect("macro expansion thread should spawn")
        .join()
        .expect("macro expansion should not panic")
}

pub(crate) fn expand_program(
    program: Program,
) -> Result<(Program, MacroAnalysis), Vec<Diagnostic>> {
    let (program, analysis, diagnostics) = expand_program_recovering(program, false)?;
    if diagnostics.is_empty() {
        Ok((program, analysis))
    } else {
        Err(diagnostics)
    }
}

/// Expands `program` and returns it alongside every expansion diagnostic.
/// With `recovering`, invalid macro definitions do not stop expansion, so the
/// result always carries a program; otherwise they fail fast with `Err`.
pub(crate) fn expand_program_recovering(
    mut program: Program,
    recovering: bool,
) -> Result<(Program, MacroAnalysis, Vec<Diagnostic>), Vec<Diagnostic>> {
    let mut expander = MacroExpander::new(&program);
    expander.validate_definitions();
    expander.validate_compile_time_code();
    for module in program.modules() {
        for item in &module.syntax.items {
            if let Item::Binding(binding) = item
                && binding.kind == BindingKind::Def
                && binding_uses_macro_call_metadata(binding)
            {
                expander.diagnostics.push(Diagnostic::new(
                    binding.syntax.span.clone(),
                    "`MacroCallMetadata` may only be the first parameter of a function-style macro",
                ));
            }
        }
    }
    if !recovering && !expander.diagnostics.is_empty() {
        return Err(expander.diagnostics);
    }

    for source_module in program.modules_mut() {
        let module = source_module.id;
        if !source_module.syntax.modifiers.is_empty() {
            let declaration = staple_syntax::Submodule {
                syntax: source_module
                    .syntax
                    .declaration_syntax
                    .clone()
                    .unwrap_or_else(|| source_module.syntax.syntax.clone()),
                docs: source_module.syntax.docs.clone(),
                visibility: source_module.syntax.visibility,
                name: String::new(),
                module: source_module.syntax.clone(),
                companion: false,
                generics_syntax: None,
                type_parameters: Vec::new(),
                trait_bounds: Vec::new(),
                subtype_bounds: Vec::new(),
                equality_bounds: Vec::new(),
                sized_relaxations: Vec::new(),
                companion_target: None,
            };
            let modified = staple_syntax::ModifiedItem {
                syntax: declaration.syntax.clone(),
                modifiers: std::mem::take(&mut source_module.syntax.modifiers),
                item: Box::new(Item::Submodule(declaration)),
            };
            match expander.apply_modifier_chain(module, modified, 0) {
                Some(ModifierChainResult::Item(Item::Submodule(result)))
                    if result.name.is_empty() =>
                {
                    let whole_file_syntax = source_module.syntax.syntax.clone();
                    source_module.syntax = result.module;
                    source_module.syntax.syntax = whole_file_syntax;
                    source_module.syntax.docs = result.docs;
                    source_module.syntax.visibility = result.visibility;
                    source_module.syntax.modifiers.clear();
                    source_module.visibility = result.visibility;
                }
                Some(_) => expander.diagnostics.push(Diagnostic::new(
                    source_module.syntax.syntax.span.clone(),
                    "a current-module modifier must return exactly one current module",
                )),
                None => {}
            }
        }
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
            ) && !expander.is_compile_time_parse_item(module, item)
        });
        if Some(module) == expander.syntax_module {
            // The default `parse_all` calls intrinsics that no longer exist
            // after expansion, so the trait keeps only its signatures.
            for item in &mut items {
                if let Item::TraitDeclaration(declaration) = item
                    && declaration.name == "Parse"
                {
                    for member in &mut declaration.members {
                        member.default = None;
                    }
                }
            }
        }
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
    program.rebuild_generated_inline_modules(&mut expander.next_syntax_id);
    program.reattach_expanded_inline_modules();
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
    let analysis = expander.analysis();
    let mut unknown_modifiers = Vec::new();
    expander.diagnostics.retain(|diagnostic| {
        if !diagnostic.message.starts_with("unknown modifier macro `") {
            return true;
        }
        let key = (diagnostic.span.clone(), diagnostic.message.clone());
        if unknown_modifiers.contains(&key) {
            false
        } else {
            unknown_modifiers.push(key);
            true
        }
    });
    Ok((program, analysis, expander.diagnostics))
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
    next_annotation_id: usize,
    next_mark: u64,
    steps: usize,
    expansion_stack: Vec<MacroKey>,
    /// One entry per active modifier application: its key and the syntax IDs
    /// of the modifiers that were still pending on its input item.
    modifier_frames: Vec<(MacroKey, Vec<SyntaxId>)>,
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
    module_packages: Vec<Option<staple_project::PackageId>>,
    active_features: staple_project::ActiveFeatures,
    declared_features: HashMap<staple_project::PackageId, HashSet<String>>,
    /// The `std/process.sta` module, so a compile-time call to the real
    /// `panic` (however it was imported/aliased) can be recognized by
    /// identity and short-circuited into a diagnostic instead of being
    /// interpreted like an ordinary function — its body calls extern `c`
    /// functions that have no compile-time meaning.
    standard_library_process: Option<ModuleId>,
    /// Call-site spans of the macro/modifier invocations currently being
    /// evaluated, innermost last. A diagnostic raised while interpreting a
    /// macro's body (such as a compile-time `panic`) should point at where
    /// the macro was *called*, not at the line inside its definition that
    /// happened to trigger the error.
    invocation_spans: Vec<Span>,
    /// Call-site span of the most recent macro or modifier invocation written
    /// directly in source (expansion depth 0). Expansion of its result runs at
    /// a greater depth, so this names the outermost invocation that led to any
    /// nested `std.source` macro call.
    root_invocation_span: Option<Span>,
    /// See `MacroAnalysis::caller_location_spans`.
    caller_location_spans: HashMap<SyntaxId, Span>,
    /// File-backed module sources, keyed by the source name their spans carry.
    source_files: HashMap<Arc<str>, SourceFile>,
    /// Set when a compile-time `panic` call has just produced a
    /// diagnostic that already names the panicking macro; consumed by the
    /// nearest enclosing macro-invocation call site so it doesn't also
    /// append a redundant "while expanding macro" trailer.
    suppress_expansion_trailer: bool,
    checked_compile_helpers: HashSet<SyntaxId>,
    checking_compile_helpers: HashSet<SyntaxId>,
    compile_binding_types: HashMap<SyntaxId, String>,
    /// Whether a statically checked call argument fills every remaining slot
    /// of a slotted parameter, keyed by the argument's syntax identity.
    /// Evaluation uses the same decision instead of guessing from value
    /// shape, so nested products bind to the same slot.
    slotted_argument_fills: HashMap<SyntaxId, bool>,
    /// The `std.syntax` module, which defines `Parse` and its intrinsics.
    syntax_module: Option<ModuleId>,
    /// The `companion F32` and `companion F64` modules. Their tier-1 float
    /// intrinsics are evaluated at compile time, and a reified non-finite
    /// float constant is rebuilt as a call to their `from_bits` intrinsic.
    float_companion_modules: [Option<ModuleId>; 2],
    /// The namespaces of `F32` and `F64`, which their names reach.
    float_namespaces: [Option<ModuleId>; 2],
    /// Every implementation of `Parse`, standard and user-written.
    parse_impls: Vec<ParseImpl>,
    trait_registry: traits::Registry,
    call_targets: HashMap<SyntaxId, CompileType>,
    call_target: Option<MetaType>,
    /// The default body of `Parse.parse_all`, with the module it was written in.
    parse_all_default: Option<(ModuleId, Expression)>,
    /// What the type parameters of the `Parse` implementation being evaluated
    /// stand for.
    type_env: TypeEnv,
    /// The value a `?` or `return` is leaving the innermost compile-time
    /// function with. Evaluation unwinds as a failure until the function call
    /// that owns it takes the value.
    pending_return: Option<Value>,
    pending_break: Option<Value>,
    pending_continue: bool,
    loop_types: Vec<Vec<CompileType>>,
    /// The caller's storage for the next call's `mut` argument, so the callee
    /// updates it in place.
    argument_place: Option<EnvironmentBinding>,
    /// The `Parse` target of each `Parse.parse`/`Parse.parse_all` call, as the
    /// static check inferred it from the call's context.
    parse_targets: HashMap<SyntaxId, ParseTarget>,
    /// While statically checking a `Parse` implementation, its type
    /// parameters and the ones bounded by `Parse`.
    static_parameters: HashSet<String>,
    static_parse_bounds: HashSet<String>,
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
        UseKind::Selected(vec![SelectedImport::new(
            declaration
                .path
                .last()
                .expect("dotted import has a final component")
                .clone(),
        )])
    } else {
        UseKind::Dotted
    }
}

impl MacroExpander {
    fn new(program: &Program) -> Self {
        let mut definitions = HashMap::new();
        let mut scopes = vec![ModuleScope::default(); program.modules().len()];
        let core = program.standard_library_core();
        let standard_library_directory = core.and_then(|id| {
            program
                .module(id)
                .path
                .parent()
                .map(std::path::Path::to_path_buf)
        });
        let prelude = program.standard_library_prelude();
        let syntax = program.standard_library_syntax();
        let number_types = program
            .modules()
            .iter()
            .find(|module| {
                module
                    .path
                    .ends_with(std::path::Path::new("std/number/types.sta"))
            })
            .map(|module| module.id);
        // The companion block written beside each float type declares its
        // intrinsics.
        let float_companion = |name: &str| {
            let module = number_types?;
            let namespace = program.child_named(module, name)?;
            program
                .companion_blocks(namespace)
                .into_iter()
                .find(|block| program.module(*block).parent == Some(module) && *block != namespace)
        };
        let float_companion_modules = [float_companion("F32"), float_companion("F64")];
        let float_namespaces = [
            number_types.and_then(|module| program.child_named(module, "F32")),
            number_types.and_then(|module| program.child_named(module, "F64")),
        ];
        let cinterop = program.standard_library_cinterop();
        let standard_library_process = program
            .modules()
            .iter()
            .find(|module| {
                module
                    .path
                    .ends_with(std::path::Path::new("std/process.sta"))
            })
            .map(|module| module.id);

        let mut next_syntax_id = program
            .modules()
            .iter()
            .map(|module| module.syntax.syntax.id.0 + 1)
            .chain([program.next_syntax_id()])
            .max()
            .unwrap_or(0);
        for source_module in program.modules() {
            for item in &source_module.syntax.items {
                let item = traits::plain_item(item);
                match item {
                    Item::MacroDeclaration(declaration) => {
                        let declaration =
                            &with_curried_macro_value(declaration, &mut next_syntax_id);
                        let key = MacroKey {
                            module: source_module.id,
                            name: declaration.name.clone(),
                            modifier: declaration.modifier,
                            syntax: declaration.syntax.id,
                        };
                        let kind =
                            if Some(source_module.id) == syntax && declaration.name == "quote" {
                                MacroKind::Quote
                            } else if Some(source_module.id) == syntax
                                && declaration.name == "parse_quote"
                            {
                                MacroKind::ParseQuote
                            } else if Some(source_module.id) == cinterop
                                && declaration.name == "__c_string_literal"
                            {
                                MacroKind::CString
                            } else if source_module
                                .path
                                .ends_with(std::path::Path::new("std/source.sta"))
                                && !declaration.modifier
                                && declaration.value.is_none()
                                && let Some(primitive) = match declaration.name.as_str() {
                                    "line" => Some(SourceLocationMacro::Line),
                                    "column" => Some(SourceLocationMacro::Column),
                                    "file" => Some(SourceLocationMacro::File),
                                    _ => None,
                                }
                            {
                                MacroKind::SourceLocation(primitive)
                            } else if source_module
                                .path
                                .ends_with(std::path::Path::new("std/annotation.sta"))
                                && declaration.modifier
                                && declaration.name == "doc"
                            {
                                MacroKind::Doc
                            } else if source_module
                                .path
                                .ends_with(std::path::Path::new("std/annotation.sta"))
                                && declaration.modifier
                                && declaration.name == "feature"
                            {
                                MacroKind::Feature
                            } else if source_module
                                .path
                                .ends_with(std::path::Path::new("std/annotation.sta"))
                                && declaration.modifier
                                && declaration.name == "no_prelude"
                            {
                                MacroKind::NoPrelude
                            } else if source_module
                                .path
                                .ends_with(std::path::Path::new("std/source.sta"))
                                && declaration.modifier
                                && declaration.name == "track_caller"
                            {
                                MacroKind::TrackCaller
                            } else if standard_library_directory.as_ref().is_some_and(|root| {
                                source_module.path == root.join("annotation.sta")
                            }) && declaration.modifier
                                && declaration.name == "sized_opaque"
                            {
                                MacroKind::SizedOpaque
                            } else if standard_library_directory.as_ref().is_some_and(|root| {
                                source_module.path == root.join("annotation.sta")
                            }) && declaration.modifier
                                && declaration.name == "recursive_constructor"
                            {
                                MacroKind::RecursiveConstructor
                            } else if let Some(value) = &declaration.value {
                                MacroKind::User(value.clone())
                            } else {
                                MacroKind::User(Expression::Product(
                                    staple_syntax::ProductExpression::empty(),
                                ))
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
                                    DelimitedMetaContents::Slotted(vec![parameters[0].clone()]),
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
                    // `Parse` and the `std.syntax` intrinsics are scope entries
                    // so they import, re-export, and rename like helpers do.
                    Item::TraitDeclaration(declaration) => {
                        scopes[source_module.id.0].helpers.insert(
                            declaration.name.clone(),
                            HelperDefinition {
                                module: source_module.id,
                                binding: Binding {
                                    syntax: declaration.syntax.clone(),
                                    docs: Vec::new(),
                                    member_modifiers: Vec::new(),
                                    visibility: declaration.visibility,
                                    kind: BindingKind::Extern,
                                    mutable: false,
                                    signal: false,
                                    name: declaration.name.clone(),
                                    annotation: None,
                                    companion_target: None,
                                    companion_generics: None,
                                    track_caller: false,
                                    value: None,
                                },
                            },
                        );
                    }
                    Item::ExternBlock(block) if block.abi == "\"staple-intrinsic\"" => {
                        for binding in &block.bindings {
                            let intrinsic = if Some(source_module.id) == syntax {
                                SyntaxIntrinsic::from_name(&binding.name)
                            } else {
                                SyntaxIntrinsic::from_float_name(&binding.name)
                            };
                            if intrinsic.is_some() || binding.kind == BindingKind::Extern {
                                let mut binding = binding.clone();
                                if block.visibility == Visibility::Public {
                                    binding.visibility = Visibility::Public;
                                }
                                scopes[source_module.id.0].helpers.insert(
                                    binding.name.clone(),
                                    HelperDefinition {
                                        module: source_module.id,
                                        binding,
                                    },
                                );
                            }
                        }
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
                    // `@doc` and `@track_caller` leave a binding's value
                    // unchanged, so it stays usable at compile time.
                    Item::Modified(modified)
                        if modified.modifiers.iter().all(|modifier| {
                            modifier.namespace.is_none()
                                && matches!(modifier.name.as_str(), "doc" | "track_caller")
                        }) =>
                    {
                        if let Item::Binding(binding) = modified.item.as_ref()
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

        // A type's namespace offers the macros and helpers of all its
        // companion blocks; each block itself sees only its own and those of
        // the module it is written in.
        for namespace in program.modules().iter().filter(|module| module.namespace) {
            let mut shared = ModuleScope::default();
            for block in program.companion_blocks(namespace.id) {
                let scope = &scopes[block.0];
                for (name, keys) in &scope.macros {
                    shared
                        .macros
                        .entry(name.clone())
                        .or_default()
                        .extend(keys.iter().cloned());
                }
                for (name, keys) in &scope.modifiers {
                    shared
                        .modifiers
                        .entry(name.clone())
                        .or_default()
                        .extend(keys.iter().cloned());
                }
                for (name, helper) in &scope.helpers {
                    shared
                        .helpers
                        .entry(name.clone())
                        .or_insert_with(|| helper.clone());
                }
            }
            let scope = &mut scopes[namespace.id.0];
            scope.macros = shared.macros;
            scope.modifiers = shared.modifiers;
            scope.helpers = shared.helpers;
        }

        let mut public_macros = HashMap::<(ModuleId, String, bool), Vec<MacroKey>>::new();
        for definition in definitions
            .values()
            .filter(|definition| definition.declaration.visibility == Visibility::Public)
        {
            public_macros
                .entry((
                    program.companion_primary(definition.key.module),
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
        // namespace, e.g. `Box.make(...)` for a macro `make` declared inside
        // `companion<T> Box<T> { ... }`. Seeded here and carried through the
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
                    && let Some(child) = program.namespace_child(submodule.syntax.id)
                {
                    public_namespaces.insert((source_module.id, submodule.name.clone()), child);
                }
            }
            for (name, namespace, visibility) in program.type_namespaces(source_module.id) {
                if visibility == Visibility::Public {
                    public_namespaces.insert((source_module.id, name), namespace);
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
                            .map(|import| (import.name.clone(), import.alias().to_owned()))
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

        let mut package_macros = public_macros.clone();
        for definition in definitions
            .values()
            .filter(|definition| definition.declaration.visibility == Visibility::Package)
        {
            package_macros
                .entry((
                    program.companion_primary(definition.key.module),
                    definition.key.name.clone(),
                    definition.key.modifier,
                ))
                .or_default()
                .push(definition.key.clone());
        }
        let mut package_helpers = public_helpers.clone();
        for module in program.modules() {
            for (name, helper) in &scopes[module.id.0].helpers {
                if helper.binding.visibility == Visibility::Package {
                    package_helpers.insert((module.id, name.clone()), helper.clone());
                }
            }
        }
        let mut package_namespaces = public_namespaces.clone();
        for module in program.modules() {
            for item in &module.syntax.items {
                if let Item::Submodule(submodule) = item
                    && submodule.visibility == Visibility::Package
                    && let Some(child) = program.namespace_child(submodule.syntax.id)
                {
                    package_namespaces.insert((module.id, submodule.name.clone()), child);
                }
            }
            for (name, namespace, visibility) in program.type_namespaces(module.id) {
                if visibility == Visibility::Package {
                    package_namespaces.insert((module.id, name), namespace);
                }
            }
        }
        loop {
            let previous_macros = package_macros.clone();
            let previous_helpers = package_helpers.clone();
            let previous_namespaces = package_namespaces.clone();
            let mut changed = false;
            for source_module in program.modules() {
                for item in &source_module.syntax.items {
                    let Item::UseDeclaration(use_) = item else {
                        continue;
                    };
                    let Some(imported) = program.imported_module(use_.syntax.id) else {
                        continue;
                    };
                    let same_package = macro_same_package(program, source_module.id, imported);
                    // A plain `pub use` glob also re-exports package-visible
                    // macros, helpers, and namespaces, but only at package
                    // visibility; the public maps never gain them.
                    let exports_package_items = match use_.visibility {
                        Visibility::Package => same_package,
                        Visibility::Public => {
                            same_package && provisional_use_kind(program, use_) == UseKind::Glob
                        }
                        Visibility::Private => false,
                    };
                    if !exports_package_items {
                        continue;
                    }
                    let names = match provisional_use_kind(program, use_) {
                        UseKind::Dotted | UseKind::Namespace => Vec::new(),
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
                            .map(|import| (import.name.clone(), import.alias().to_owned()))
                            .collect(),
                        UseKind::Renamed { item, alias } => vec![(item.clone(), alias.clone())],
                    };
                    for (item, alias) in names {
                        for modifier in [false, true] {
                            if let Some(keys) =
                                previous_macros.get(&(imported, item.clone(), modifier))
                            {
                                changed |= package_macros
                                    .insert(
                                        (source_module.id, alias.clone(), modifier),
                                        keys.clone(),
                                    )
                                    .is_none();
                            }
                        }
                        if let Some(helper) = previous_helpers.get(&(imported, item.clone())) {
                            changed |= package_helpers
                                .insert((source_module.id, alias.clone()), helper.clone())
                                .is_none();
                        }
                        if let Some(namespace) = previous_namespaces.get(&(imported, item)) {
                            changed |= package_namespaces
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
                    let child = program.namespace_child(submodule.syntax.id)?;
                    Some(((module.id, submodule.name.clone()), child))
                })
            })
            .chain(program.modules().iter().flat_map(|module| {
                program
                    .type_namespaces(module.id)
                    .into_iter()
                    .map(move |(name, namespace, _)| ((module.id, name), namespace))
            }))
            .collect::<HashMap<_, _>>();

        for source_module in program.modules() {
            if let Some(core) = core
                && source_module.id != core
            {
                // The implicit import behaves like a private `use std.core.*`, so
                // same-package modules also see its package-visible items.
                let (macros, helpers, namespaces) =
                    if macro_same_package(program, core, source_module.id) {
                        (&package_macros, &package_helpers, &package_namespaces)
                    } else {
                        (&public_macros, &public_helpers, &public_namespaces)
                    };
                for ((module, name, modifier), keys) in macros {
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
                for ((module, name), binding) in helpers {
                    if *module == core {
                        scopes[source_module.id.0]
                            .helpers
                            .entry(name.clone())
                            .or_insert_with(|| binding.clone());
                    }
                }
                for ((module, name), target) in namespaces {
                    if *module == core {
                        scopes[source_module.id.0]
                            .namespaces
                            .entry(name.clone())
                            .or_insert_with(|| *target);
                    }
                }
            }
            if program.module_uses_prelude(source_module.id)
                && let Some(prelude) = prelude
                && source_module.id != prelude
            {
                for ((module, name, modifier), keys) in &public_macros {
                    if *module != prelude {
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
                    let scope = &mut scopes[source_module.id.0];
                    // As for a glob: the module's own macro of the same name
                    // must not be hidden by a prelude value.
                    if *module == prelude
                        && !scope.macros.contains_key(name)
                        && !scope.modifiers.contains_key(name)
                    {
                        scope
                            .helpers
                            .entry(name.clone())
                            .or_insert_with(|| binding.clone());
                    }
                }
                for ((module, name), target) in &public_namespaces {
                    if *module == prelude {
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
                    && let Some(child) = program.namespace_child(submodule.syntax.id)
                {
                    scopes[source_module.id.0]
                        .namespaces
                        .insert(submodule.name.clone(), child);
                }
            }
            for (name, namespace, _) in program.type_namespaces(source_module.id) {
                scopes[source_module.id.0]
                    .namespaces
                    .insert(name, namespace);
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
                } else if use_.visibility != Visibility::Public
                    && macro_same_package(program, imported, source_module.id)
                {
                    (&package_macros, &package_helpers, &package_namespaces)
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
                            let scope = &mut scopes[source_module.id.0];
                            // A glob never shadows a name the scope already
                            // has. Helpers are looked up before macros, so a
                            // glob-imported value would otherwise hide the
                            // module's own macro of the same name.
                            if *module == imported
                                && !scope.macros.contains_key(name)
                                && !scope.modifiers.contains_key(name)
                            {
                                scope
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
                        for import in names {
                            Self::install_selected(
                                &mut scopes[source_module.id.0],
                                imported,
                                &import.name,
                                import.alias(),
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

        let mut expander = Self {
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
            next_annotation_id: 0,
            next_mark: 1,
            steps: 0,
            expansion_stack: Vec::new(),
            modifier_frames: Vec::new(),
            emitted_items: None,
            invocations: HashMap::new(),
            quote_context: None,
            helper_eval_depth: 0,
            module_packages: program
                .modules()
                .iter()
                .map(|module| program.package_of(module.id))
                .collect(),
            active_features: program.active_features().clone(),
            declared_features: program
                .package_graph()
                .map(|graph| {
                    graph
                        .packages
                        .iter()
                        .enumerate()
                        .map(|(index, package)| {
                            (
                                staple_project::PackageId(index),
                                package.features.keys().cloned().collect(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            standard_library_process,
            suppress_expansion_trailer: false,
            checked_compile_helpers: HashSet::new(),
            checking_compile_helpers: HashSet::new(),
            compile_binding_types: HashMap::new(),
            slotted_argument_fills: HashMap::new(),
            invocation_spans: Vec::new(),
            root_invocation_span: None,
            caller_location_spans: HashMap::new(),
            source_files: source_files(program),
            syntax_module: syntax,
            float_companion_modules,
            float_namespaces,
            parse_impls: Vec::new(),
            trait_registry: traits::Registry::default(),
            call_targets: HashMap::new(),
            call_target: None,
            parse_all_default: None,
            type_env: TypeEnv::default(),
            pending_return: None,
            pending_break: None,
            pending_continue: false,
            loop_types: Vec::new(),
            argument_place: None,
            parse_targets: HashMap::new(),
            static_parameters: HashSet::new(),
            static_parse_bounds: HashSet::new(),
        };
        expander.trait_registry = traits::Registry::collect(program, &expander);
        let (impls, default, diagnostics) = parse::collect_parse_impls(program, &expander);
        expander.parse_impls = impls;
        expander.parse_all_default = default;
        expander.diagnostics.extend(diagnostics);
        expander
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
            binding_types: self.compile_binding_types.clone(),
            compile_time_items: self
                .syntax_module
                .map(|module| {
                    self.scopes[module.0]
                        .helpers
                        .values()
                        .filter(|helper| self.helper_intrinsic(helper).is_some())
                        .map(|helper| (module, helper.binding.name.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            caller_location_spans: self.caller_location_spans.clone(),
            next_syntax_id: self.next_syntax_id,
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

    /// Statically validates code which can run in the macro interpreter.
    /// This deliberately runs before the first expansion: otherwise an
    /// unused bad definition would remain silent and a used one would fail
    /// only on the path exercised by that invocation.
    fn validate_compile_time_code(&mut self) {
        let definitions = self.definitions.values().cloned().collect::<Vec<_>>();
        for definition in definitions {
            let MacroKind::User(body) = &definition.kind else {
                continue;
            };
            if definition.declaration.value.is_none() {
                continue;
            }
            let mut expected = CompileType::Meta(definition.result.clone());
            for (index, parameter) in definition.parameters.iter().enumerate().rev() {
                let parameter = if definition.key.modifier
                    && index + 1 != definition.parameters.len()
                {
                    match modifier_argument_meta_type(parameter) {
                        Some(MetaType::Delimited(_, DelimitedMetaContents::Separated { element, separator })) => {
                            CompileType::Meta(MetaType::Nominal("Separated".to_owned(), vec![element.as_ref().clone(), separator.as_ref().clone()]))
                        }
                        Some(inner) => CompileType::Meta(inner.clone()),
                        None => CompileType::Meta(parameter.clone()),
                    }
                } else {
                    CompileType::Meta(parameter.clone())
                };
                expected = CompileType::Function(Box::new(parameter), Box::new(expected));
            }
            let mut environment = CompileEnvironment::new();
            self.check_compile_expression(
                definition.key.module,
                body,
                &mut environment,
                Some(&expected),
            );
        }

        let helpers = self
            .scopes
            .iter()
            .flat_map(|scope| scope.helpers.values())
            .filter(|helper| binding_is_compile_time_helper(&helper.binding))
            .cloned()
            .collect::<Vec<_>>();
        for helper in helpers {
            self.check_compile_helper(&helper);
        }
        self.check_parse_impls();
    }

    fn check_compile_helper(&mut self, helper: &HelperDefinition) {
        let syntax = helper.binding.syntax.id;
        if self.checked_compile_helpers.contains(&syntax)
            || !self.checking_compile_helpers.insert(syntax)
            || (Some(helper.module) == self.standard_library_process
                && helper.binding.name == "panic")
        {
            return;
        }
        if let Some(value) = &helper.binding.value {
            let expected = binding_signature(&helper.binding)
                .map(|ty| self.checked_compile_type(&ty))
                .unwrap_or(CompileType::Unknown);
            self.check_compile_expression(
                helper.module,
                value,
                &mut CompileEnvironment::new(),
                (expected != CompileType::Unknown).then_some(&expected),
            );
        }
        self.checking_compile_helpers.remove(&syntax);
        self.checked_compile_helpers.insert(syntax);
    }

    fn check_compile_expression(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        environment: &mut CompileEnvironment,
        expected: Option<&CompileType>,
    ) -> CompileType {
        if let Some(expected) = expected {
            self.call_targets
                .insert(expression.syntax().id, expected.clone());
        }
        if self
            .trait_member(module, expression, |name| environment.contains_key(name))
            .is_some()
        {
            return CompileType::Unknown;
        }
        let actual = match expression {
            Expression::Function(function) => {
                let (parameter, result) = match expected {
                    Some(CompileType::Function(parameter, result)) => {
                        (parameter.as_ref().clone(), Some(result.as_ref()))
                    }
                    _ => (
                        compile_pattern_annotation(&function.pattern)
                            .unwrap_or(CompileType::Unknown),
                        None,
                    ),
                };
                let mut nested = environment.clone();
                self.bind_compile_pattern(module, &function.pattern, &parameter, &mut nested);
                let outer_loops = std::mem::take(&mut self.loop_types);
                let body =
                    self.check_compile_expression(module, &function.body, &mut nested, result);
                self.loop_types = outer_loops;
                CompileType::Function(Box::new(parameter), Box::new(body))
            }
            Expression::TypeApplication(application) => {
                self.check_compile_expression(module, &application.value, environment, expected)
            }
            Expression::TypeAscription(value) => {
                let annotation = self.checked_compile_type(&value.ty);
                self.check_compile_expression(module, &value.value, environment, Some(&annotation))
            }
            Expression::Match(value) => {
                let mut subject =
                    self.check_compile_expression(module, &value.subject, environment, None);
                // A parenthesized `Result` subject is the `Result` itself.
                if let CompileType::Product(elements) | CompileType::SlottedProduct(elements) =
                    &subject
                    && let [CompileType::Result(..)] = elements.as_slice()
                {
                    subject = elements[0].clone();
                }
                let mut previous_patterns = Vec::new();
                let mut result = CompileType::Never;
                for arm in &value.arms {
                    if !compile_pattern_is_useful(&previous_patterns, &arm.pattern) {
                        self.diagnostics.push(Diagnostic::new(
                            arm.pattern.syntax().span.clone(),
                            "unreachable match arm",
                        ));
                    }
                    previous_patterns.push(&arm.pattern);
                    let mut nested = environment.clone();
                    self.bind_compile_pattern(module, &arm.pattern, &subject, &mut nested);
                    let arm_type =
                        self.check_compile_expression(module, &arm.body, &mut nested, expected);
                    result = join_compile_types(result, arm_type);
                }
                if !matches!(subject, CompileType::Unknown | CompileType::Error)
                    && !compile_type_contains_unknown(&subject)
                    && !compile_patterns_are_exhaustive(&subject, &previous_patterns)
                {
                    self.diagnostics.push(Diagnostic::new(
                        value.syntax.span.clone(),
                        "non-exhaustive match",
                    ));
                }
                result
            }
            Expression::Block(block) => {
                let mut nested = environment.clone();
                let mut result = CompileType::Product(Vec::new());
                let last = block.items.len().saturating_sub(1);
                for (index, item) in block.items.iter().enumerate() {
                    result = self.check_compile_item(
                        module,
                        item,
                        &mut nested,
                        (index == last).then_some(expected).flatten(),
                    );
                }
                result
            }
            // `(value)` is `value`, as it is at run time.
            Expression::Product(product)
                if product.elements.len() == 1
                    && product.elements[0].name.is_none()
                    && !product.elements[0].spread =>
            {
                self.check_compile_expression(module, &product.elements[0].value, environment, None)
            }
            Expression::Product(product) => {
                let elements = product
                    .elements
                    .iter()
                    .map(|element| {
                        self.check_compile_expression(module, &element.value, environment, None)
                    })
                    .collect();
                if product.slotted {
                    CompileType::SlottedProduct(elements)
                } else {
                    CompileType::Product(elements)
                }
            }
            Expression::RepeatedProduct(value) => {
                let element =
                    self.check_compile_expression(module, &value.value, environment, None);
                let count = self.checked_compile_type(&value.count);
                self.require_compile_type(
                    &count,
                    &CompileType::Integer,
                    value.count.syntax().span.clone(),
                );
                CompileType::Product(vec![element])
            }
            Expression::Call(_) => {
                self.check_compile_call(module, expression, environment, expected)
            }
            Expression::Access(access) => {
                if matches!(access.accessor, Accessor::Method(_)) {
                    return CompileType::Unknown;
                }
                if let (Expression::Name(ty), Accessor::Name(member)) =
                    (access.value.as_ref(), &access.accessor)
                    && ty.name.chars().next().is_some_and(char::is_uppercase)
                    && self.trait_registry.companion_member_exists(member)
                {
                    return CompileType::Unknown;
                }
                if let Some(helper) = self.qualified_compile_helper(module, access) {
                    self.check_compile_helper(&helper);
                    helper_compile_type(&helper.binding)
                } else {
                    let value =
                        self.check_compile_expression(module, &access.value, environment, None);
                    compile_field_type(&value, &access.accessor).unwrap_or_else(|| {
                        let message = if value == CompileType::Meta(MetaType::CallExpr) {
                            format!(
                                "call syntax has no field `{}`",
                                accessor_name(&access.accessor)
                            )
                        } else {
                            format!(
                                "compile-time value has no field `{}`",
                                accessor_name(&access.accessor)
                            )
                        };
                        self.diagnostics
                            .push(Diagnostic::new(access.syntax.span.clone(), message));
                        CompileType::Error
                    })
                }
            }
            Expression::Index(index) => {
                let value = self.check_compile_expression(module, &index.value, environment, None);
                let position =
                    self.check_compile_expression(module, &index.index, environment, None);
                self.require_compile_type(
                    &position,
                    &CompileType::Integer,
                    index.index.syntax().span.clone(),
                );
                match value {
                    CompileType::Product(elements) | CompileType::SlottedProduct(elements) => {
                        elements
                            .into_iter()
                            .reduce(join_compile_types)
                            .unwrap_or(CompileType::Unknown)
                    }
                    CompileType::Error => CompileType::Error,
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            index.value.syntax().span.clone(),
                            "compile-time indexing requires a product",
                        ));
                        CompileType::Error
                    }
                }
            }
            Expression::Binary(binary) => {
                let left = self.check_compile_expression(module, &binary.left, environment, None);
                let right = self.check_compile_expression(module, &binary.right, environment, None);
                match binary.operator {
                    BinaryOperator::Equal
                    | BinaryOperator::NotEqual
                    | BinaryOperator::Less
                    | BinaryOperator::LessEqual
                    | BinaryOperator::Greater
                    | BinaryOperator::GreaterEqual => {
                        self.require_compile_type(
                            &right,
                            &left,
                            binary.right.syntax().span.clone(),
                        );
                        CompileType::Bool
                    }
                    BinaryOperator::Add
                    | BinaryOperator::Subtract
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Remainder => {
                        self.require_compile_type(
                            &right,
                            &left,
                            binary.right.syntax().span.clone(),
                        );
                        left
                    }
                    BinaryOperator::And | BinaryOperator::Or => {
                        self.require_compile_type(
                            &left,
                            &CompileType::Bool,
                            binary.left.syntax().span.clone(),
                        );
                        self.require_compile_type(
                            &right,
                            &CompileType::Bool,
                            binary.right.syntax().span.clone(),
                        );
                        CompileType::Bool
                    }
                    BinaryOperator::Range | BinaryOperator::RangeInclusive => CompileType::Unknown,
                }
            }
            Expression::Unary(unary) => match unary.operator {
                staple_syntax::UnaryOperator::Negate => {
                    self.check_compile_expression(module, &unary.operand, environment, None)
                }
                staple_syntax::UnaryOperator::Not => {
                    let operand = self.check_compile_expression(
                        module,
                        &unary.operand,
                        environment,
                        Some(&CompileType::Bool),
                    );
                    self.require_compile_type(
                        &operand,
                        &CompileType::Bool,
                        unary.operand.syntax().span.clone(),
                    );
                    CompileType::Bool
                }
            },
            Expression::Logical(logical) => {
                let left = self.check_compile_expression(
                    module,
                    &logical.left,
                    environment,
                    Some(&CompileType::Bool),
                );
                let right = self.check_compile_expression(
                    module,
                    &logical.right,
                    environment,
                    Some(&CompileType::Bool),
                );
                self.require_compile_type(
                    &left,
                    &CompileType::Bool,
                    logical.left.syntax().span.clone(),
                );
                self.require_compile_type(
                    &right,
                    &CompileType::Bool,
                    logical.right.syntax().span.clone(),
                );
                CompileType::Bool
            }
            Expression::Quote(quote) => {
                self.check_quote_splices(module, quote, environment);
                match quote.kind {
                    staple_syntax::QuoteKind::Quote => CompileType::Meta(MetaType::Syntax),
                    staple_syntax::QuoteKind::ParseQuote => match expected {
                        Some(CompileType::Meta(meta)) if !quote_result_type(meta) => {
                            self.diagnostics.push(Diagnostic::new(
                                quote.syntax.span.clone(),
                                format!(
                                    "{} is not a supported `parse_quote` context",
                                    format_meta_type(meta)
                                ),
                            ));
                            CompileType::Error
                        }
                        Some(ty @ CompileType::Meta(_)) => ty.clone(),
                        _ => inferred_compile_quote_type(quote),
                    },
                }
            }
            Expression::Name(name) => self.compile_name_type(module, name, environment),
            Expression::Char(_) => CompileType::Named("Char".to_owned()),
            Expression::String(_) => CompileType::String,
            Expression::Integer(_) => CompileType::Integer,
            Expression::Float(_) => CompileType::Float,
            Expression::VisibilityArgument(_) => CompileType::Meta(MetaType::Visibility),
            Expression::Splice(splice) => {
                self.diagnostics.push(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "splices are only allowed inside `quote`",
                ));
                CompileType::Error
            }
            Expression::SyntaxArgument(argument) => {
                self.diagnostics.push(Diagnostic::new(
                    argument.syntax.span.clone(),
                    "grouped type or pattern syntax may only be passed to a matching macro parameter",
                ));
                CompileType::Error
            }
            Expression::StringTemplate(value) => {
                self.diagnostics.push(Diagnostic::new(
                    value.syntax.span.clone(),
                    "string templates are not available during compile-time macro evaluation",
                ));
                CompileType::Error
            }
            Expression::Resource(value) => {
                self.diagnostics.push(Diagnostic::new(
                    value.syntax.span.clone(),
                    "resources are not available during compile-time macro evaluation",
                ));
                CompileType::Error
            }
            Expression::With(value) => {
                self.diagnostics.push(Diagnostic::new(
                    value.syntax.span.clone(),
                    "resources are not available during compile-time macro evaluation",
                ));
                CompileType::Error
            }
            Expression::Loop(value) => {
                self.loop_types.push(Vec::new());
                self.check_compile_expression(
                    module,
                    &Expression::Block(value.body.clone()),
                    environment,
                    None,
                );
                let breaks = self.loop_types.pop().unwrap();
                breaks
                    .into_iter()
                    .reduce(|left, right| {
                        if left == right {
                            left
                        } else {
                            CompileType::Unknown
                        }
                    })
                    .unwrap_or(CompileType::Product(Vec::new()))
            }
            Expression::Coro(value) => {
                self.diagnostics.push(Diagnostic::new(
                    value.syntax.span.clone(),
                    "coroutines are not available during compile-time macro evaluation",
                ));
                CompileType::Error
            }
            Expression::Await(value) => {
                self.diagnostics.push(Diagnostic::new(
                    value.syntax.span.clone(),
                    "`await` is not available during compile-time macro evaluation",
                ));
                CompileType::Error
            }
            Expression::Try(value) => {
                // `Ok x` is `x`; any other outcome leaves the function.
                match self.check_compile_expression(module, &value.operand, environment, None) {
                    CompileType::Result(ok, _) => *ok,
                    CompileType::Error => CompileType::Error,
                    _ => CompileType::Unknown,
                }
            }
            Expression::Conversion(value) => {
                self.diagnostics.push(Diagnostic::new(
                    value.syntax.span.clone(),
                    "`as` is not available during compile-time macro evaluation",
                ));
                CompileType::Error
            }
            Expression::Pipe(value) => {
                let lowered = lower_pipe_expression(value.clone(), &mut self.next_syntax_id);
                self.check_compile_expression(module, &lowered, environment, expected)
            }
            Expression::CString(_) => CompileType::Named("CString".to_owned()),
        };
        if let Some(expected) = expected
            && !matches!(expression, Expression::Quote(quote) if quote.kind == staple_syntax::QuoteKind::Quote)
        {
            self.require_compile_type(&actual, expected, expression.syntax().span.clone());
        }
        actual
    }

    fn check_compile_item(
        &mut self,
        module: ModuleId,
        item: &Item,
        environment: &mut CompileEnvironment,
        expected: Option<&CompileType>,
    ) -> CompileType {
        match item {
            Item::Binding(binding) => {
                let annotation =
                    binding_signature(binding).map(|ty| self.checked_compile_type(&ty));
                let ty = binding.value.as_ref().map_or(CompileType::Error, |value| {
                    self.check_compile_expression(module, value, environment, annotation.as_ref())
                });
                environment.insert(
                    binding.name.clone(),
                    CompileBinding {
                        ty: annotation.unwrap_or(ty),
                        mutable: binding.mutable,
                    },
                );
                CompileType::Product(Vec::new())
            }
            Item::PatternBinding(binding) => {
                let ty = self.check_compile_expression(module, &binding.value, environment, None);
                self.bind_compile_pattern(module, &binding.pattern, &ty, environment);
                CompileType::Product(Vec::new())
            }
            Item::Assignment(assignment) => {
                let target =
                    self.check_compile_expression(module, &assignment.target, environment, None);
                self.check_compile_expression(
                    module,
                    &assignment.value,
                    environment,
                    Some(&target),
                );
                if let Expression::Name(name) = &assignment.target
                    && environment
                        .get(&name.name)
                        .is_some_and(|binding| !binding.mutable)
                {
                    self.diagnostics.push(Diagnostic::new(
                        assignment.target.syntax().span.clone(),
                        format!(
                            "cannot assign to immutable compile-time binding `{}`",
                            name.name
                        ),
                    ));
                }
                CompileType::Product(Vec::new())
            }
            Item::Expression(value) => {
                self.check_compile_expression(module, value, environment, expected)
            }
            Item::Return(value) => {
                self.check_compile_expression(module, &value.value, environment, expected)
            }
            Item::Break(value) => {
                let ty = value
                    .value
                    .as_ref()
                    .map(|value| {
                        self.check_compile_expression(module, value, environment, expected)
                    })
                    .unwrap_or(CompileType::Product(Vec::new()));
                if let Some(breaks) = self.loop_types.last_mut() {
                    breaks.push(ty);
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        value.syntax.span.clone(),
                        "`break` requires a loop",
                    ));
                }
                CompileType::Unknown
            }
            Item::Continue(value) => {
                if self.loop_types.is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        value.syntax.span.clone(),
                        "`continue` requires a loop",
                    ));
                }
                CompileType::Unknown
            }
            other => {
                self.diagnostics.push(Diagnostic::new(
                    other.syntax().span.clone(),
                    "this item is not supported during compile-time evaluation",
                ));
                CompileType::Error
            }
        }
    }

    fn check_compile_call(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        environment: &mut CompileEnvironment,
        expected: Option<&CompileType>,
    ) -> CompileType {
        if let Expression::Call(call) = expression {
            if let Some((member, type_arguments)) =
                self.parse_member_head(module, &call.callee, |name| environment.contains_key(name))
            {
                return self.check_parse_call(
                    module,
                    call,
                    member,
                    type_arguments,
                    environment,
                    expected,
                );
            }
            if self
                .trait_member(module, &call.callee, |name| environment.contains_key(name))
                .is_some()
            {
                self.check_compile_expression(module, &call.argument, environment, None);
                return CompileType::Unknown;
            }
            if let Expression::TypeApplication(application) = call.callee.as_ref() {
                return self.check_type_applied_call(module, call, application, environment);
            }
        }
        let (head, arguments) = flatten_call(expression);
        if let Expression::Name(name) = head {
            if !environment.contains_key(&name.name)
                && let Some(helper) = self.lookup_helper(module, &name.name)
                && matches!(&helper.binding.value, Some(Expression::Function(function)) if function.generics.is_some())
            {
                self.check_compile_helper(&helper);
                for argument in &arguments {
                    self.check_compile_expression(module, argument, environment, None);
                }
                return CompileType::Unknown;
            }
            if self
                .lookup_helper(module, &name.name)
                .and_then(|helper| self.helper_intrinsic(&helper))
                == Some(SyntaxIntrinsic::SequencePush)
            {
                for argument in &arguments {
                    self.check_compile_expression(module, argument, environment, None);
                }
                return CompileType::Unknown;
            }
        }
        if let Some(keys) = self.resolve_macro(module, head) {
            // Match overloads the same way real call sites and compile-time
            // evaluation do, so function-style macros with greedy `Expr *`
            // parameters and a trailing `Sequence<Expr>` (`if`, `while`, `for`,
            // `when`) are usable inside compile-time helper and macro bodies.
            // `select_macro` reports its own diagnostic when nothing matches.
            let Some(selected) =
                self.select_macro(&keys, &arguments, None, expression.syntax().span.clone())
            else {
                return CompileType::Error;
            };
            if selected.consumed < arguments.len() {
                // Excess arguments would be applied to the expansion as calls;
                // that is only meaningful for function-producing macros, which
                // are not expressible as a compile-time meta type. Leave the
                // result unconstrained rather than guessing.
                for argument in &arguments[selected.consumed..] {
                    self.check_compile_expression(module, argument, environment, None);
                }
                return CompileType::Unknown;
            }
            // A macro whose result is raw `Syntax`/`SyntaxNode` yields an
            // expression value here, the same coercion `parse_quote` performs:
            // adopt the expected quotable meta type when one is supplied.
            let result = selected.definition.result.clone();
            // A control-flow macro such as `if` or `when` is evaluated in
            // place, so a `Result` context says nothing about its syntax type.
            if matches!(expected, Some(CompileType::Result(..)))
                && matches!(
                    result,
                    MetaType::Syntax | MetaType::SyntaxNode | MetaType::Expr
                )
            {
                return CompileType::Unknown;
            }
            if matches!(result, MetaType::Syntax | MetaType::SyntaxNode) {
                match expected {
                    Some(CompileType::Meta(meta)) if quote_result_type(meta) => {
                        return CompileType::Meta(meta.clone());
                    }
                    _ => return CompileType::Meta(MetaType::Expr),
                }
            }
            return CompileType::Meta(result);
        }

        if let Expression::Name(name) = head
            && name.name.chars().next().is_some_and(char::is_uppercase)
        {
            let argument = arguments
                .first()
                .map(|argument| self.check_compile_expression(module, argument, environment, None))
                .unwrap_or_else(|| CompileType::Product(Vec::new()));
            return compile_constructor_type(&name.name, argument);
        }

        let mut callee = self.check_compile_expression(module, head, environment, None);
        if let Some(first) = arguments.first()
            && self.head_takes_mut(module, head, environment)
        {
            self.require_mutable_argument(first, environment);
        }
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index];
            if callee == CompileType::Unknown {
                for argument in &arguments[index..] {
                    self.check_compile_expression(module, argument, environment, None);
                }
                return CompileType::Unknown;
            }
            let CompileType::Function(parameter, result) = callee else {
                if callee != CompileType::Error && callee != CompileType::Unknown {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        "cannot call this compile-time value",
                    ));
                }
                return CompileType::Error;
            };
            // A slotted parameter consumes one juxtaposed argument per slot,
            // exactly like a runtime function. A whole slotted product value
            // fills every remaining slot at once.
            let Some(mut slots) = compile_function_slots(&parameter) else {
                self.check_compile_expression(module, argument, environment, Some(&parameter));
                callee = *result;
                index += 1;
                continue;
            };
            let multiple_slots = slots.len() > 1;
            if multiple_slots && compile_argument_fills_slots(argument, environment, &slots) {
                self.slotted_argument_fills
                    .insert(argument.syntax().id, true);
                let expected = CompileType::SlottedProduct(slots);
                self.check_compile_expression(module, argument, environment, Some(&expected));
                callee = *result;
                index += 1;
                continue;
            }
            if multiple_slots {
                self.slotted_argument_fills
                    .insert(argument.syntax().id, false);
            }
            let slot = slots.remove(0);
            self.check_compile_expression(module, argument, environment, Some(&slot));
            callee = compile_function_after_slots(slots, *result);
            index += 1;
        }
        callee
    }

    fn compile_name_type(
        &mut self,
        module: ModuleId,
        name: &staple_syntax::NameExpression,
        environment: &CompileEnvironment,
    ) -> CompileType {
        if let Some(binding) = environment.get(&name.name) {
            return binding.ty.clone();
        }
        let mut cursor = name
            .syntax
            .definition_module()
            .map(ModuleId)
            .unwrap_or(module);
        loop {
            if let Some(helper) = self.scopes[cursor.0].helpers.get(&name.name).cloned() {
                self.check_compile_helper(&helper);
                return helper_compile_type(&helper.binding);
            }
            if !self.companion_modules[cursor.0] {
                break;
            }
            let Some(parent) = self.parent_modules[cursor.0] else {
                break;
            };
            cursor = parent;
        }
        if let Some(ty) = compile_builtin_name_type(&name.name) {
            return ty;
        }
        self.diagnostics.push(Diagnostic::new(
            name.syntax.span.clone(),
            format!("compile-time name `{}` is not available", name.name),
        ));
        CompileType::Error
    }

    fn qualified_compile_helper(
        &self,
        module: ModuleId,
        access: &staple_syntax::AccessExpression,
    ) -> Option<HelperDefinition> {
        let (namespace, item, definition_module) = qualified_macro_access_path(access)?;
        let mut context = definition_module.map(ModuleId).unwrap_or(module);
        loop {
            if let Some(target) = self.scopes[context.0].namespaces.get(&namespace) {
                return self.scopes[target.0].helpers.get(&item).cloned();
            }
            if !self.companion_modules[context.0] {
                return None;
            }
            context = self.parent_modules[context.0]?;
        }
    }

    fn bind_compile_pattern(
        &mut self,
        module: ModuleId,
        pattern: &Pattern,
        ty: &CompileType,
        environment: &mut CompileEnvironment,
    ) {
        match pattern {
            Pattern::At(at) => {
                self.record_compile_binding_type(at.binding.syntax.id, ty);
                environment.insert(
                    at.binding.name.clone(),
                    CompileBinding {
                        ty: ty.clone(),
                        mutable: at.binding.mutable,
                    },
                );
                self.bind_compile_pattern(module, &at.pattern, ty, environment);
            }
            Pattern::Binding(binding)
                if !binding.name.chars().next().is_some_and(char::is_uppercase) =>
            {
                let bound = if matches!(binding.ty, Type::Inferred(_)) {
                    ty.clone()
                } else {
                    let annotation = self.checked_compile_type(&binding.ty);
                    let compatible = compile_types_compatible(&annotation, ty);
                    self.require_compile_type(&annotation, ty, binding.syntax.span.clone());
                    if compatible {
                        annotation
                    } else {
                        CompileType::Error
                    }
                };
                self.record_compile_binding_type(binding.syntax.id, &bound);
                environment.insert(
                    binding.name.clone(),
                    CompileBinding {
                        ty: bound,
                        mutable: binding.mutable,
                    },
                );
            }
            Pattern::Product(product) => {
                let fields = compile_product_fields(ty, product.elements.len());
                for (index, element) in product.elements.iter().enumerate() {
                    self.bind_compile_pattern(
                        module,
                        element,
                        fields.get(index).unwrap_or(&CompileType::Unknown),
                        environment,
                    );
                }
            }
            Pattern::Nominal(nominal) => {
                let representation = compile_constructor_representation(ty, &nominal.name)
                    .unwrap_or(CompileType::Unknown);
                self.bind_compile_pattern(module, &nominal.argument, &representation, environment);
            }
            Pattern::Wildcard(_)
            | Pattern::StringLiteral(_)
            | Pattern::Splice(_)
            | Pattern::Binding(_) => {}
        }
    }

    fn record_compile_binding_type(&mut self, syntax: SyntaxId, ty: &CompileType) {
        if !matches!(ty, CompileType::Error | CompileType::Unknown) {
            self.compile_binding_types
                .insert(syntax, format_compile_type(ty));
        }
    }

    fn check_quote_splices(
        &mut self,
        module: ModuleId,
        quote: &staple_syntax::QuoteExpression,
        environment: &CompileEnvironment,
    ) {
        match &quote.template {
            staple_syntax::QuoteTemplate::Expression(value) => {
                self.check_quoted_expression(module, value, environment)
            }
            staple_syntax::QuoteTemplate::Item(item) => {
                self.check_quoted_item(module, item, environment)
            }
            staple_syntax::QuoteTemplate::Items(items) => {
                for item in items {
                    self.check_quoted_item(module, item, environment);
                }
            }
            staple_syntax::QuoteTemplate::Raw => {
                for token in quote.contents.tokens().windows(2) {
                    if token[0].kind == staple_syntax::TokenKind::Dollar
                        && token[1].kind == staple_syntax::TokenKind::Identifier
                        && !environment.contains_key(&token[1].text)
                    {
                        self.diagnostics.push(Diagnostic::new(
                            quote.syntax.span.clone(),
                            format!("unknown splice `${}`", token[1].text),
                        ));
                    }
                }
            }
        }
    }

    fn check_quoted_expression(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        environment: &CompileEnvironment,
    ) {
        match expression {
            Expression::Splice(splice) => {
                if !environment.contains_key(&splice.name) {
                    self.diagnostics.push(Diagnostic::new(
                        splice.syntax.span.clone(),
                        format!("unknown splice `${}`", splice.name),
                    ));
                }
            }
            Expression::Function(value) => {
                self.check_quoted_expression(module, &value.body, environment)
            }
            Expression::TypeApplication(value) => {
                self.check_quoted_expression(module, &value.value, environment)
            }
            Expression::TypeAscription(value) => {
                self.check_quoted_expression(module, &value.value, environment)
            }
            Expression::Match(value) => {
                self.check_quoted_expression(module, &value.subject, environment);
                for arm in &value.arms {
                    self.check_quoted_expression(module, &arm.body, environment);
                }
            }
            Expression::Block(value)
            | Expression::Loop(staple_syntax::LoopExpression { body: value, .. })
            | Expression::Coro(staple_syntax::CoroExpression { body: value, .. }) => {
                for item in &value.items {
                    self.check_quoted_item(module, item, environment);
                }
            }
            Expression::Await(value) => {
                self.check_quoted_expression(module, &value.operand, environment)
            }
            Expression::Try(value) => {
                self.check_quoted_expression(module, &value.operand, environment)
            }
            Expression::Conversion(value) => {
                self.check_quoted_expression(module, &value.value, environment)
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    self.check_quoted_expression(module, &element.value, environment);
                }
            }
            Expression::RepeatedProduct(value) => {
                self.check_quoted_expression(module, &value.value, environment);
            }
            Expression::Call(value) => {
                self.check_quoted_expression(module, &value.callee, environment);
                self.check_quoted_expression(module, &value.argument, environment);
            }
            Expression::Access(value) => {
                self.check_quoted_expression(module, &value.value, environment)
            }
            Expression::Index(value) => {
                self.check_quoted_expression(module, &value.value, environment);
                self.check_quoted_expression(module, &value.index, environment);
            }
            Expression::Unary(value) => {
                self.check_quoted_expression(module, &value.operand, environment)
            }
            Expression::Binary(value) => {
                self.check_quoted_expression(module, &value.left, environment);
                self.check_quoted_expression(module, &value.right, environment);
            }
            Expression::Pipe(value) => {
                self.check_quoted_expression(module, &value.receiver, environment);
                for argument in value.stage_arguments() {
                    self.check_quoted_expression(module, argument, environment);
                }
            }
            Expression::Logical(value) => {
                self.check_quoted_expression(module, &value.left, environment);
                self.check_quoted_expression(module, &value.right, environment);
            }
            Expression::Quote(value) => self.check_quote_splices(module, value, environment),
            Expression::With(value) => {
                self.check_quoted_expression(module, &value.value, environment);
                for item in &value.body.items {
                    self.check_quoted_item(module, item, environment);
                }
            }
            _ => {}
        }
    }

    fn check_quoted_item(
        &mut self,
        module: ModuleId,
        item: &Item,
        environment: &CompileEnvironment,
    ) {
        match item {
            Item::Binding(value) => {
                if let Some(value) = &value.value {
                    self.check_quoted_expression(module, value, environment);
                }
            }
            Item::PatternBinding(value) => {
                self.check_quoted_expression(module, &value.value, environment)
            }
            Item::Assignment(value) => {
                self.check_quoted_expression(module, &value.target, environment);
                self.check_quoted_expression(module, &value.value, environment);
            }
            Item::Return(value) => self.check_quoted_expression(module, &value.value, environment),
            Item::Break(value) => {
                if let Some(value) = &value.value {
                    self.check_quoted_expression(module, value, environment);
                }
            }
            Item::Expression(value) => self.check_quoted_expression(module, value, environment),
            _ => {}
        }
    }

    fn checked_compile_type(&mut self, ty: &Type) -> CompileType {
        reject_compile_forall(ty, &mut self.diagnostics);
        compile_type(ty)
    }

    fn require_compile_type(&mut self, actual: &CompileType, expected: &CompileType, span: Span) {
        if !compile_types_compatible(actual, expected) {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "expected `{}`, found `{}`",
                    format_compile_type(expected),
                    format_compile_type(actual)
                ),
            ));
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
            if let Some(annotation) = &definition.declaration.annotation {
                let before = self.diagnostics.len();
                reject_compile_forall(annotation, &mut self.diagnostics);
                if self.diagnostics.len() != before {
                    continue;
                }
            }
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
                .any(|parameter| matches!(parameter, MetaType::SlottedProduct(_)))
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "slotted product syntax shapes may only appear inside delimited contents",
                ));
            }
            if let Some(entries) = &definition.declaration.modifier_vocabulary {
                if definition.key.modifier
                    || definition.parameters.first() != Some(&MetaType::MacroCallMetadata)
                {
                    self.diagnostics.push(Diagnostic::new(definition.declaration.syntax.span.clone(), "a modifier vocabulary requires a function-style macro whose first parameter is `MacroCallMetadata`"));
                }
                for entry in entries {
                    if entry
                        .argument
                        .as_ref()
                        .is_some_and(|ty| vocabulary_argument_meta(ty).is_none())
                    {
                        self.diagnostics.push(Diagnostic::new(
                            entry.syntax.span.clone(),
                            "invalid modifier vocabulary argument type",
                        ));
                    }
                }
            }
            validate_top_level_sequence(definition, &mut self.diagnostics);
            if definition.parameters.iter().any(invalid_raw_syntax_shape) {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "opaque `Syntax` may be a top-level argument or the entire contents of a delimiter, but cannot be partitioned by `Sequence`, `Separated`, `Option`, or product shapes",
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
                        && body_parameter.is_some_and(|body_parameter| {
                            body_parameter != *effective_declared
                                && !(effective_declared == &MetaType::MacroCallMetadata
                                    && matches!(
                                        body_parameter,
                                        MetaType::SlottedProduct(_) | MetaType::Product(_)
                                    ))
                        })
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
                    "recursive_constructor"
                        | "doc"
                        | "feature"
                        | "no_prelude"
                        | "track_caller"
                        | "sized_opaque"
                ) && !matches!(
                    definition.kind,
                    MacroKind::Doc
                        | MacroKind::Feature
                        | MacroKind::NoPrelude
                        | MacroKind::TrackCaller
                        | MacroKind::SizedOpaque
                        | MacroKind::RecursiveConstructor
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
                        modifier_argument_meta_type(leading).is_some_and(valid_modifier_argument)
                    }
                    _ => false,
                };
                let valid_result = matches!(definition.result, MetaType::Item | MetaType::Syntax)
                    || matches!(&definition.result, MetaType::Sequence(inner) if **inner == MetaType::Item);
                if !valid_parameters || !valid_result {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "modifier macro `@{}` must have signature `Item -> Item`, `Item -> Sequence<Item>`, or `Item -> Syntax`, optionally with a leading `Parenthesized<Expr | Type | Pattern> ->` argument",
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
                    *parameter == MetaType::Visibility
                        || meta_type_contains_macro_call_metadata(parameter)
                })
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "modifier macros do not accept visibility or macro-call metadata parameters",
                ));
            }
            if !definition.key.modifier
                && (definition
                    .parameters
                    .iter()
                    .enumerate()
                    .any(|(index, parameter)| {
                        meta_type_contains_macro_call_metadata(parameter)
                            && (index != 0 || *parameter != MetaType::MacroCallMetadata)
                    })
                    || meta_type_contains_macro_call_metadata(&definition.result))
            {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    format!(
                        "macro `{}` may use `MacroCallMetadata` only as its first parameter",
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
                MacroKind::SourceLocation(primitive)
                    if definition.parameters != [MetaType::Product(Vec::new())]
                        || definition.result != MetaType::Expr =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "compiler-provided macro `{}` must have signature `() -> Expr`",
                            primitive.name()
                        ),
                    ));
                }
                MacroKind::SizedOpaque
                | MacroKind::RecursiveConstructor
                | MacroKind::TrackCaller
                    if definition.parameters != [MetaType::Item]
                        || definition.result != MetaType::Item
                        || definition.declaration.value.is_some() =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        definition.declaration.syntax.span.clone(),
                        format!(
                            "compiler-provided modifier `@{}` must be bodyless with signature `Item -> Item`",
                            definition.key.name
                        ),
                    ));
                }
                MacroKind::Doc
                | MacroKind::Feature
                | MacroKind::NoPrelude
                | MacroKind::TrackCaller
                | MacroKind::SizedOpaque
                | MacroKind::RecursiveConstructor => {}
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
                self.expand_macro_call_metadata_invocation(module, invocation, depth)
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
                self.root_invocation_span = Some(modified.syntax.span.clone());
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
            // `Parse` implementations are compile-time code, like macro bodies:
            // they run in the interpreter and are dropped after expansion.
            Item::TraitImplementation(implementation)
                if self.names_parse_trait(module, &implementation.trait_name)
                    || implementation.arguments.iter().any(type_contains_syntax) => {}
            Item::TraitImplementation(implementation) => {
                for associated in &mut implementation.associated_types {
                    self.apply_member_modifiers(
                        &mut associated.modifiers,
                        &mut associated.docs,
                        MemberKind::Implementation,
                    );
                }
                for member in &mut implementation.members {
                    self.apply_member_modifiers(
                        &mut member.modifiers,
                        &mut member.docs,
                        MemberKind::Implementation,
                    );
                    member.value = self.expand_expression(module, member.value.clone(), depth);
                }
            }
            Item::TraitDeclaration(declaration) => {
                for associated in &mut declaration.associated_types {
                    self.apply_member_modifiers(
                        &mut associated.modifiers,
                        &mut associated.docs,
                        MemberKind::TraitType,
                    );
                }
                for member in &mut declaration.members {
                    member.track_caller |= self.apply_member_modifiers(
                        &mut member.modifiers,
                        &mut member.docs,
                        MemberKind::TraitFunction,
                    );
                    if let Some(default) = member.default.take() {
                        member.default = Some(self.expand_expression(module, default, depth));
                    }
                }
            }
            Item::ExternBlock(block) => {
                for binding in &mut block.bindings {
                    self.apply_member_modifiers(
                        &mut binding.member_modifiers,
                        &mut binding.docs,
                        MemberKind::Other,
                    );
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
            | Item::TypeDeclaration(_) => {}
        }
    }

    /// Interprets the `@doc` and `@track_caller` modifiers written on a trait,
    /// implementation, or `extern` member: `@doc` text joins the member's
    /// `///` documentation, and the result reports `@track_caller`. The
    /// modifiers are consumed, so reapplying is a no-op.
    fn apply_member_modifiers(
        &mut self,
        modifiers: &mut Vec<staple_syntax::ModifierInvocation>,
        docs: &mut Vec<String>,
        kind: MemberKind,
    ) -> bool {
        let mut track_caller = false;
        for modifier in std::mem::take(modifiers) {
            let span = modifier.syntax.span.clone();
            if modifier.namespace.is_none() && modifier.name == "track_caller" {
                let message = match kind {
                    MemberKind::TraitFunction if modifier.argument.is_none() => {
                        track_caller = true;
                        continue;
                    }
                    MemberKind::TraitFunction => Some("`@track_caller` takes no argument"),
                    MemberKind::TraitType => {
                        Some("`@track_caller` may only modify a trait member function")
                    }
                    MemberKind::Implementation => Some(
                        "`@track_caller` belongs on the trait member; implementation members inherit it",
                    ),
                    MemberKind::Other => None,
                };
                if let Some(message) = message {
                    self.diagnostics.push(Diagnostic::new(span, message));
                    continue;
                }
            }
            if modifier.namespace.is_some() || modifier.name != "doc" {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    match kind {
                        MemberKind::TraitFunction | MemberKind::TraitType => {
                            "only `@doc` and `@track_caller` may modify a trait member"
                        }
                        _ => "only `@doc` may modify a declaration member",
                    },
                ));
                continue;
            }
            let Some(argument) = modifier.argument else {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`@doc` requires a parenthesized string literal",
                ));
                continue;
            };
            let Some(Expression::String(literal)) = argument.expression else {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`@doc` requires a string literal argument",
                ));
                continue;
            };
            match staple_syntax::string_literal::decode(&literal.literal) {
                Ok(doc) => docs.push(doc),
                Err(message) => self.diagnostics.push(Diagnostic::new(span, message)),
            }
        }
        track_caller
    }

    fn expand_macro_call_metadata_invocation(
        &mut self,
        module: ModuleId,
        invocation: staple_syntax::VisibilityMacroInvocation,
        depth: usize,
    ) -> Option<Item> {
        if depth == 0 {
            self.root_invocation_span = Some(invocation.syntax.span.clone());
        }
        let (head, arguments) = flatten_call(&invocation.expression);
        let Some(keys) = self.resolve_macro(module, head) else {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "a modifier or visibility prefix must precede a macro whose first parameter is `MacroCallMetadata`",
            ));
            return None;
        };
        let selected = self.select_macro(
            &keys,
            &arguments,
            Some(&MacroCallMetadata {
                syntax: invocation.syntax.clone(),
                modifiers: invocation.modifiers.clone(),
                visibility: invocation.visibility.clone(),
            }),
            invocation.syntax.span.clone(),
        )?;
        self.record_invocation(head.syntax().id, &selected.definition);
        if let Some(entries) = &selected.definition.declaration.modifier_vocabulary {
            for prefix in &invocation.modifiers {
                if let Some(entry) = entries
                    .iter()
                    .find(|entry| prefix.namespace.is_none() && entry.name == prefix.name)
                {
                    let valid = match (&entry.argument, &prefix.argument) {
                        (None, None) => true,
                        (Some(ty), Some(argument)) => vocabulary_argument_meta(ty)
                            .is_some_and(|expected| modifier_argument_matches(&expected, argument)),
                        _ => false,
                    };
                    if !valid {
                        self.diagnostics.push(Diagnostic::new(
                            prefix.syntax.span.clone(),
                            format!(
                                "invalid argument for modifier `@{}` of macro `{}`",
                                prefix.name, selected.definition.key.name
                            ),
                        ));
                        return None;
                    }
                } else if self
                    .resolve_modifier(module, prefix)
                    .is_none_or(|keys| keys.is_empty())
                {
                    self.diagnostics.push(Diagnostic::new(
                        prefix.syntax.span.clone(),
                        format!(
                            "macro `{}` does not accept modifier `@{}`",
                            selected.definition.key.name, prefix.name
                        ),
                    ));
                    return None;
                }
            }
        }
        if depth >= MAX_EXPANSION_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "macro expansion exceeded the limit of 128 nested expansions",
            ));
            return None;
        };
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
                if self.suppress_expansion_trailer {
                    self.suppress_expansion_trailer = false;
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span,
                        format!("while expanding macro `{}`", key.name),
                    ));
                }
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
                        expression = Expression::Call(staple_syntax::CallExpression {
                            spread: false,
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

    /// Resolves the documentation text carried by a single `@doc`/`///`
    /// modifier invocation, recording the invocation when it names the
    /// explicit `@doc(...)` modifier.
    fn resolve_doc_modifier_text(
        &mut self,
        module: ModuleId,
        invocation: &staple_syntax::ModifierInvocation,
    ) -> Option<String> {
        if invocation.argument.is_some() {
            let (definition, _) = self.select_modifier(module, invocation)?;
            self.record_invocation(invocation.syntax.id, &definition);
        }
        if let Some(doc) = invocation.doc.clone() {
            return Some(doc);
        }
        let Some(argument) = invocation.argument.as_ref() else {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "`@doc` requires a parenthesized string literal",
            ));
            return None;
        };
        let Some(Expression::String(literal)) = argument.expression.as_ref() else {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "`@doc` requires a string literal argument",
            ));
            return None;
        };
        match staple_syntax::string_literal::decode(&literal.literal) {
            Ok(doc) => Some(doc),
            Err(message) => {
                self.diagnostics
                    .push(Diagnostic::new(invocation.syntax.span.clone(), message));
                None
            }
        }
    }

    fn apply_modifier_chain(
        &mut self,
        module: ModuleId,
        mut modified: staple_syntax::ModifiedItem,
        depth: usize,
    ) -> Option<ModifierChainResult> {
        let Some(invocation) = modified.modifiers.first().cloned() else {
            self.diagnostics.push(Diagnostic::new(
                modified.syntax.span,
                "modified items must contain at least one modifier",
            ));
            return None;
        };
        modified.modifiers.remove(0);
        let mut current = if modified.modifiers.is_empty() {
            *modified.item
        } else {
            Item::Modified(modified)
        };
        if let Item::VisibilityMacroInvocation(generated) = current {
            current = self.expand_macro_call_metadata_invocation(module, generated, depth + 1)?;
        }
        if !modifier_target_supported(&current)
            && !(invocation.namespace.is_none()
                && matches!(invocation.name.as_str(), "doc" | "no_prelude"))
        {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "modifier macros may only be applied to `let`, `def`, `type`, `extern`, `trait`, or `impl` items",
            ));
            return None;
        }
        if depth >= MAX_EXPANSION_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                "macro expansion exceeded the limit of 128 nested expansions",
            ));
            return None;
        }
        if invocation.namespace.is_none() && invocation.name == "no_prelude" {
            let (definition, _) = self.select_modifier(module, &invocation)?;
            self.record_invocation(invocation.syntax.id, &definition);
            if invocation.argument.is_some() {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@no_prelude` does not accept an argument",
                ));
                return None;
            }
            if !matches!(&current, Item::Submodule(value) if value.name.is_empty()) {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@no_prelude` may only modify the current module declaration",
                ));
                return None;
            }
            return Some(ModifierChainResult::Item(current));
        }
        if invocation.namespace.is_none() && invocation.name == "doc" {
            // A run of leading `@doc`/`///` modifiers describes one declaration
            // and must be attached in source order. `apply_modifier_chain`
            // processes the outermost (topmost) modifier first and `attach_doc`
            // inserts at the front, so handling the run one modifier per pass
            // would reverse it. Peel the whole run out of the still-unprocessed
            // modifier list and attach it in a single step instead.
            let mut doc_invocations = vec![invocation];
            if let Item::Modified(rest) = &mut current {
                while rest
                    .modifiers
                    .first()
                    .is_some_and(|next| next.namespace.is_none() && next.name == "doc")
                {
                    doc_invocations.push(rest.modifiers.remove(0));
                }
            }
            current = match current {
                Item::Modified(rest) if rest.modifiers.is_empty() => *rest.item,
                other => other,
            };
            let mut docs = Vec::with_capacity(doc_invocations.len());
            for doc_invocation in &doc_invocations {
                docs.push(self.resolve_doc_modifier_text(module, doc_invocation)?);
            }
            for doc in docs.into_iter().rev() {
                if !attach_doc(&mut current, doc) {
                    self.diagnostics.push(Diagnostic::new(
                        doc_invocations[0].syntax.span.clone(),
                        "`@doc` may only modify a named declaration",
                    ));
                    return None;
                }
            }
            return Some(ModifierChainResult::Item(current));
        }
        if invocation.namespace.is_none() && invocation.name == "feature" {
            let (definition, _) = self.select_modifier(module, &invocation)?;
            self.record_invocation(invocation.syntax.id, &definition);
            let Some(argument) = invocation.argument.as_ref() else {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@feature` requires a parenthesized string literal",
                ));
                return None;
            };
            let Some(Expression::String(literal)) = argument.expression.as_ref() else {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@feature` requires a string literal argument",
                ));
                return None;
            };
            let feature = match staple_syntax::string_literal::decode(&literal.literal) {
                Ok(feature) => feature,
                Err(message) => {
                    self.diagnostics
                        .push(Diagnostic::new(invocation.syntax.span, message));
                    return None;
                }
            };
            let Some(package) = self.module_packages[module.0] else {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@feature` requires a package manifest",
                ));
                return None;
            };
            if !self
                .declared_features
                .get(&package)
                .is_some_and(|features| features.contains(&feature))
            {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    format!("package does not declare feature `{feature}`"),
                ));
                return None;
            }
            return if self
                .active_features
                .get(&package)
                .is_some_and(|features| features.contains(&feature))
            {
                Some(ModifierChainResult::Item(current))
            } else {
                Some(ModifierChainResult::Items(Vec::new()))
            };
        }
        let (definition, argument) = self.select_modifier(module, &invocation)?;
        self.record_invocation(invocation.syntax.id, &definition);
        if matches!(definition.kind, MacroKind::RecursiveConstructor) {
            let Some(declaration) = modified_type_declaration_mut(&mut current) else {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@recursive_constructor` may only modify a type declaration",
                ));
                return None;
            };
            declaration.recursive_constructor = true;
            return Some(ModifierChainResult::Item(current));
        }
        if matches!(definition.kind, MacroKind::TrackCaller) {
            let message = "`@track_caller` may only modify a `def` whose value is a function";
            if argument.is_some() {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@track_caller` takes no argument",
                ));
                return None;
            }
            let Some(binding) = modified_binding_mut(&mut current) else {
                self.diagnostics
                    .push(Diagnostic::new(invocation.syntax.span, message));
                return None;
            };
            if binding.kind != staple_syntax::BindingKind::Def
                || !matches!(binding.value, Some(Expression::Function(_)))
            {
                self.diagnostics
                    .push(Diagnostic::new(invocation.syntax.span, message));
                return None;
            }
            binding.track_caller = true;
            return Some(ModifierChainResult::Item(current));
        }
        if matches!(definition.kind, MacroKind::SizedOpaque) {
            let Some(declaration) = modified_type_declaration_mut(&mut current) else {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@sized_opaque` may only modify an opaque type declaration",
                ));
                return None;
            };
            if declaration.kind() != staple_syntax::TypeDeclarationKind::Opaque {
                self.diagnostics.push(Diagnostic::new(
                    invocation.syntax.span,
                    "`@sized_opaque` may only modify an opaque type declaration",
                ));
                return None;
            }
            declaration.sized_opaque = true;
            return Some(ModifierChainResult::Item(current));
        }
        let key = definition.key.clone();
        // Re-entering a modifier is recursion unless the invocation was already
        // pending on the input of every active application of that modifier,
        // which is the case for repeated prefixes such as `@x @x def ...` that
        // the outer application merely passed through. A modifier that emits
        // a new invocation of itself is always reported.
        let recursive = self
            .modifier_frames
            .iter()
            .any(|(active, pending)| *active == key && !pending.contains(&invocation.syntax.id));
        if recursive {
            self.diagnostics.push(Diagnostic::new(
                invocation.syntax.span.clone(),
                format!("recursive modifier macro expansion of `@{}`", key.name),
            ));
            return None;
        }
        let pending = match &current {
            Item::Modified(pending) => pending
                .modifiers
                .iter()
                .map(|modifier| modifier.syntax.id)
                .collect(),
            _ => Vec::new(),
        };
        self.expansion_stack.push(key.clone());
        self.modifier_frames.push((key.clone(), pending));
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
            self.modifier_frames.pop();
            if self.diagnostics.len() > diagnostic_start {
                if self.suppress_expansion_trailer {
                    self.suppress_expansion_trailer = false;
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        invocation.syntax.span.clone(),
                        format!("while expanding modifier macro `@{}`", key.name),
                    ));
                }
            }
            return None;
        };
        let mut expanded = Vec::new();
        for item in items {
            if let Item::Modified(nested) = item {
                match self.apply_modifier_chain(module, nested, depth + 1) {
                    Some(ModifierChainResult::Item(item)) => expanded.push(item),
                    Some(ModifierChainResult::Items(items)) => expanded.extend(items),
                    None => {
                        self.expansion_stack.pop();
                        self.modifier_frames.pop();
                        return None;
                    }
                }
            } else {
                expanded.push(item);
            }
        }
        self.expansion_stack.pop();
        self.modifier_frames.pop();
        match expanded.as_slice() {
            [item] => Some(ModifierChainResult::Item(item.clone())),
            _ => Some(ModifierChainResult::Items(expanded)),
        }
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
                                let definition = &self.definitions[*key];
                                definition.declaration.visibility == Visibility::Public
                                    || (definition.declaration.visibility == Visibility::Package
                                        && self.module_packages[context.0]
                                            .zip(self.module_packages[definition.key.module.0])
                                            .is_some_and(|(caller, owner)| caller == owner))
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
    ) -> Option<(MacroDefinition, Option<Value>)> {
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
    ) -> Option<Value> {
        parse_modifier_argument_value(expected, argument, &mut self.next_syntax_id)
    }

    fn invoke_modifier(
        &mut self,
        module: ModuleId,
        definition: &MacroDefinition,
        argument: Option<Value>,
        item: Item,
        call_span: Span,
    ) -> Option<Vec<Item>> {
        self.invocation_spans.push(call_span.clone());
        let result = self.invoke_modifier_inner(module, definition, argument, item, call_span);
        self.invocation_spans.pop();
        result
    }

    fn invoke_modifier_inner(
        &mut self,
        module: ModuleId,
        definition: &MacroDefinition,
        argument: Option<Value>,
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
                value = self.apply_value(value, argument, call_span.clone())?;
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
                let mut items = match staple_syntax::parse_item_list_fragment(
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
                        "modifier macro `@{}` must return `Item`, `Sequence<Item>`, or `Syntax`, but returned {} syntax",
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
                        "modifier macro `@{}` did not return `Item`, `Sequence<Item>`, or `Syntax`",
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
            self.root_invocation_span = Some(expression.syntax().span.clone());
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
                if self.suppress_expansion_trailer {
                    self.suppress_expansion_trailer = false;
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        format!("while expanding macro `{}`", key.name),
                    ));
                }
            }
            return true;
        };

        match result {
            SyntaxValue::Raw(raw) => {
                let parsed =
                    staple_syntax::parse_item_list_fragment(&raw, &mut self.next_syntax_id);
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
                            result = Expression::Call(staple_syntax::CallExpression {
                                spread: false,
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
                        result = Expression::Call(staple_syntax::CallExpression {
                            spread: false,
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
                    let annotation = binding.annotation_body();
                    let float =
                        annotation.and_then(|annotation| self.named_float_type(module, annotation));
                    let folded = self
                        .eval_expression(module, value, &mut Environment::new())
                        .and_then(|result| self.assign_float_type(result, float, span.clone()))
                        .and_then(|result| {
                            self.value_to_expression(result, span, module, annotation)
                        });
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
            if depth == 0 {
                self.root_invocation_span = Some(modified.syntax.span.clone());
            }
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
                for import in names {
                    install(&import.name, import.alias(), scope);
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
                self.root_invocation_span = Some(expression.syntax().span.clone());
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
                    if self.suppress_expansion_trailer {
                        self.suppress_expansion_trailer = false;
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            expression.syntax().span.clone(),
                            format!("while expanding macro `{}`", key.name),
                        ));
                    }
                }
                return expression;
            };
            let result = if let SyntaxValue::Raw(raw) = result {
                match staple_syntax::parse_expression_fragment(&raw, &mut self.next_syntax_id) {
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
                result = Expression::Call(staple_syntax::CallExpression {
                    spread: false,
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
                    | "Angled"
                    | "Braced"
                    | "Private"
                    | "Package"
                    | "Public"
                    | "PublicReprPackage"
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
            Expression::TypeApplication(mut application) => {
                application.value =
                    Box::new(self.expand_expression(module, *application.value, depth));
                Expression::TypeApplication(application)
            }
            Expression::TypeAscription(mut ascription) => {
                ascription.value =
                    Box::new(self.expand_expression(module, *ascription.value, depth));
                Expression::TypeAscription(ascription)
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
            Expression::RepeatedProduct(mut repeated) => {
                repeated.value = Box::new(self.expand_expression(module, *repeated.value, depth));
                Expression::RepeatedProduct(repeated)
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
            Expression::Unary(mut unary) => {
                unary.operand = Box::new(self.expand_expression(module, *unary.operand, depth));
                Expression::Unary(unary)
            }
            Expression::Binary(mut binary) => {
                binary.left = Box::new(self.expand_expression(module, *binary.left, depth));
                binary.right = Box::new(self.expand_expression(module, *binary.right, depth));
                Expression::Binary(binary)
            }
            Expression::Pipe(mut pipe) => {
                pipe.receiver = Box::new(self.expand_expression(module, *pipe.receiver, depth));
                // Only the stage's arguments are expressions in scope; its head
                // names a method and is never a macro invocation.
                for argument in pipe.stage_arguments_mut() {
                    let expanded = self.expand_expression(
                        module,
                        std::mem::replace(
                            argument,
                            Expression::Product(staple_syntax::ProductExpression::empty()),
                        ),
                        depth,
                    );
                    *argument = expanded;
                }
                Expression::Pipe(pipe)
            }
            Expression::Logical(mut logical) => {
                logical.left = Box::new(self.expand_expression(module, *logical.left, depth));
                logical.right = Box::new(self.expand_expression(module, *logical.right, depth));
                Expression::Logical(logical)
            }
            Expression::StringTemplate(mut template) => {
                for part in &mut template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        interpolation.expression = Box::new(self.expand_expression(
                            module,
                            (*interpolation.expression).clone(),
                            depth,
                        ));
                    }
                }
                Expression::StringTemplate(template)
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
            Expression::Coro(mut coro) => {
                self.expand_block(module, &mut coro.body, depth);
                Expression::Coro(coro)
            }
            Expression::Await(mut await_) => {
                await_.operand = Box::new(self.expand_expression(module, *await_.operand, depth));
                Expression::Await(await_)
            }
            Expression::Try(mut try_) => {
                try_.operand = Box::new(self.expand_expression(module, *try_.operand, depth));
                Expression::Try(try_)
            }
            Expression::Conversion(mut conversion) => {
                conversion.value =
                    Box::new(self.expand_expression(module, *conversion.value, depth));
                Expression::Conversion(conversion)
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
        call_metadata: Option<&MacroCallMetadata>,
        span: Span,
    ) -> Option<SelectedMacro> {
        let definitions = keys
            .iter()
            .filter_map(|key| self.definitions.get(key).cloned())
            .collect::<Vec<_>>();
        let mut complete = definitions
            .iter()
            .filter_map(|definition| {
                match_macro_arguments(definition, arguments, call_metadata).map(
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
            if definitions.iter().any(|definition| definition.parameters.iter().any(|parameter| {
                matches!(parameter, MetaType::TypeParameters)
                    || matches!(parameter, MetaType::Optional(inner) if **inner == MetaType::TypeParameters)
            })) {
                for argument in arguments {
                    if argument.syntax().tokens().iter().find(|token| !token.kind.is_trivia()).is_some_and(|token| token.text == "<") {
                        if let Err(error) = staple_syntax::parse_type_parameters_prefix(argument.syntax(), &mut 0) {
                            self.diagnostics.push(Diagnostic::new(argument.syntax().span.clone(), error.message));
                            return None;
                        }
                    }
                }
            }
            let has_visibility_parameters = definitions.iter().any(|definition| {
                definition.parameters.iter().any(|parameter| {
                    matches!(
                        parameter,
                        MetaType::Visibility | MetaType::MacroCallMetadata
                    )
                })
            });
            if call_metadata.is_some() {
                let name = keys
                    .first()
                    .map(|key| key.name.as_str())
                    .unwrap_or("<macro>");
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "macro `{name}` has no overload whose first parameter is `MacroCallMetadata`"
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

    /// Expands `std.source.line`, `column`, or `file`. A call written at the
    /// outermost invocation site reports its own location; one produced by a
    /// macro definition reports the outermost invocation that led to it.
    fn expand_source_location(
        &mut self,
        primitive: SourceLocationMacro,
        call_span: Span,
    ) -> Option<SyntaxValue> {
        let span = match &self.root_invocation_span {
            Some(root) if !span_contains(root, &call_span) => root.clone(),
            _ => call_span,
        };
        let Span::User {
            source: Some(source),
            range,
            location,
        } = &span
        else {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "`{}` could not determine the source location of its invocation",
                    primitive.name()
                ),
            ));
            return None;
        };
        let Some(file) = self.source_files.get_mut(source) else {
            self.diagnostics.push(Diagnostic::new(
                span.clone(),
                format!(
                    "`{}` could not determine the source file of its invocation",
                    primitive.name()
                ),
            ));
            return None;
        };
        let expression = match primitive {
            SourceLocationMacro::Line | SourceLocationMacro::Column => {
                let location = location.unwrap_or_else(|| {
                    let text = file.text.get_or_insert_with(|| {
                        file.tokens
                            .iter()
                            .map(|token| token.text.as_str())
                            .collect()
                    });
                    source_location_at(text, range.start)
                });
                let value = if primitive == SourceLocationMacro::Line {
                    location.line
                } else {
                    location.column
                };
                Expression::Integer(staple_syntax::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: value.to_string(),
                })
            }
            SourceLocationMacro::File => {
                let literal = staple_syntax::string_literal::encode(&file.path);
                Expression::String(staple_syntax::StringExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal,
                })
            }
        };
        Some(SyntaxValue::from_expression(expression))
    }

    fn invoke_macro(
        &mut self,
        definition: &MacroDefinition,
        arguments: Vec<MacroArgument>,
        call_span: Span,
    ) -> Option<SyntaxValue> {
        self.invocation_spans.push(call_span.clone());
        let result = self.invoke_macro_inner(definition, arguments, call_span);
        self.invocation_spans.pop();
        result
    }

    fn invoke_macro_inner(
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
                    staple_syntax::CStringExpression {
                        syntax,
                        literal: string.literal.clone(),
                    },
                )))
            }
            MacroKind::SourceLocation(primitive) => {
                self.expand_source_location(*primitive, call_span)
            }
            MacroKind::Doc
            | MacroKind::Feature
            | MacroKind::NoPrelude
            | MacroKind::TrackCaller
            | MacroKind::SizedOpaque
            | MacroKind::RecursiveConstructor => {
                unreachable!("built-in modifiers are applied directly")
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
                            MacroArgument::Parsed(argument) => {
                                value = self.apply_value(value, argument, call_span.clone())?;
                                continue;
                            }
                            // A product parameter receives a parenthesized,
                            // comma-separated argument as a product of syntax.
                            MacroArgument::Expression(argument)
                                if matches!(expected, MetaType::Product(_)) =>
                            {
                                let argument = match_syntax_fragment(
                                    expected,
                                    argument.syntax(),
                                    &mut self.next_syntax_id,
                                )?;
                                value = self.apply_value(value, argument, call_span.clone())?;
                                continue;
                            }
                            MacroArgument::Expression(argument) => {
                                self.meta_argument_value(expected, &argument)?
                            }
                            MacroArgument::Visibility(visibility) => {
                                SyntaxValue::Visibility(visibility)
                            }
                            MacroArgument::Metadata(metadata) => {
                                value = self.apply_value(
                                    value,
                                    Value::Product(
                                        vec![
                                            (
                                                Some("modifiers".to_owned()),
                                                Value::Sequence(
                                                    metadata
                                                        .modifiers
                                                        .iter()
                                                        .enumerate()
                                                        .map(|(index, invocation)| {
                                                            Value::Syntax(SyntaxValue::Modifier(
                                                                OpaqueModifier {
                                                                    invocation: invocation.clone(),
                                                                    enclosing: (index == 0).then(
                                                                        || metadata.syntax.clone(),
                                                                    ),
                                                                },
                                                            ))
                                                        })
                                                        .collect(),
                                                ),
                                            ),
                                            (
                                                Some("visibility".to_owned()),
                                                Value::Syntax(SyntaxValue::Visibility(
                                                    metadata.visibility,
                                                )),
                                            ),
                                        ]
                                        .into(),
                                        false,
                                    ),
                                    call_span.clone(),
                                )?;
                                continue;
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
                                    Value::Sequence(values.into()),
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
                MetaType::Syntax | MetaType::SyntaxNode | MetaType::Visibility
            )
            .then(|| SyntaxValue::Visibility(visibility.clone()));
        }
        match expected {
            MetaType::TypeParameter | MetaType::TypeParameters => {
                let Value::Syntax(value) =
                    match_syntax_fragment(expected, argument.syntax(), &mut self.next_syntax_id)?
                else {
                    return None;
                };
                Some(value)
            }
            MetaType::Type => {
                parse_type_argument(argument, &mut self.next_syntax_id).map(SyntaxValue::Type)
            }
            MetaType::Pattern => {
                parse_pattern_argument(argument, &mut self.next_syntax_id).map(SyntaxValue::Pattern)
            }
            MetaType::Visibility => {
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
                    MetaType::CharExpr => "a character-literal expression".to_owned(),
                    MetaType::StringExpr => "a string-literal expression".to_owned(),
                    MetaType::UnstructuredExpr => "an unstructured expression".to_owned(),
                    MetaType::Type => "a type".to_owned(),
                    MetaType::Pattern => "a pattern".to_owned(),
                    MetaType::BindingPattern => "a binding pattern".to_owned(),
                    MetaType::NominalPattern => "a nominal pattern".to_owned(),
                    MetaType::Item => "an item".to_owned(),
                    MetaType::Modifier => "a modifier".to_owned(),
                    MetaType::ModifierCall => "a modifier call".to_owned(),
                    MetaType::UnstructuredModifier => "an unstructured modifier".to_owned(),
                    MetaType::ModifiedItem => "a modified item".to_owned(),
                    MetaType::TypeDeclarationItem => "a type declaration item".to_owned(),
                    MetaType::TypeBody => "a type declaration body".to_owned(),
                    MetaType::TypeParameter => "a type parameter".to_owned(),
                    MetaType::TypeParameters => "type parameters".to_owned(),
                    MetaType::ResourceDependency => "a resource dependency".to_owned(),
                    MetaType::UnstructuredItem => "an unstructured item".to_owned(),
                    MetaType::Visibility => "visibility syntax".to_owned(),
                    MetaType::MacroCallMetadata => "macro-call metadata".to_owned(),
                    MetaType::Comma => "comma syntax".to_owned(),
                    MetaType::Equals => "equals syntax".to_owned(),
                    MetaType::FatArrow => "fat-arrow syntax".to_owned(),
                    MetaType::SlottedProduct(_)
                    | MetaType::Product(_)
                    | MetaType::Optional(_)
                    | MetaType::Nominal(_, _)
                    | MetaType::Sequence(_) => {
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
        let target = self
            .call_targets
            .get(&expression.syntax().id)
            .and_then(|ty| parse::compile_type_target(ty, &self.type_env));
        self.eval_expression_target(module, expression, environment, target)
    }

    /// An expected result belongs to this expression, not to its arguments or
    /// unrelated statements evaluated inside it.
    fn eval_expression_target(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        environment: &mut Environment,
        target: Option<MetaType>,
    ) -> Option<Value> {
        let previous = std::mem::replace(&mut self.call_target, target);
        let result = self.eval_expression_inner(module, expression, environment);
        self.call_target = previous;
        result
    }

    fn eval_expression_inner(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        environment: &mut Environment,
    ) -> Option<Value> {
        if !self.tick(expression.syntax().span.clone()) {
            return None;
        }
        if let Some((key, member)) =
            self.trait_member(module, expression, |name| environment.contains_key(name))
        {
            return Some(Value::TraitMember(key, member, Vec::new(), Vec::new()));
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
                result_target: None,
                type_env: self.type_env.clone(),
            }),
            Expression::TypeApplication(application) => {
                let value = self.eval_expression(module, &application.value, environment)?;
                Some(Value::TypeApplied(
                    Box::new(value),
                    application.arguments.clone(),
                ))
            }
            Expression::TypeAscription(ascription) => {
                if let Some(expected) = meta_type(&ascription.ty)
                    && let Expression::Quote(quote) = ascription.value.as_ref()
                    && quote.kind == staple_syntax::QuoteKind::ParseQuote
                {
                    if !quote_result_type(&expected) {
                        self.diagnostics.push(Diagnostic::new(
                            ascription.ty.syntax().span.clone(),
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
                let target = self.resolve_type(&ascription.ty);
                let value =
                    self.eval_expression_target(module, &ascription.value, environment, target)?;
                let float = self.named_float_type(module, &ascription.ty);
                self.assign_float_type(value, float, ascription.syntax.span.clone())
            }
            Expression::Binary(binary) => self.eval_binary_expression(module, binary, environment),
            Expression::Unary(unary) => self.eval_unary_expression(module, unary, environment),
            Expression::Quote(quote) => {
                let expected = match quote.kind {
                    staple_syntax::QuoteKind::Quote => MetaType::Syntax,
                    staple_syntax::QuoteKind::ParseQuote => match self.quote_context.clone() {
                        Some(expected) => expected,
                        None => {
                            self.diagnostics.push(Diagnostic::new(
                                quote.syntax.span.clone(),
                                "`parse_quote` requires a contextual syntax type; annotate the binding, use `T :: expression`, or declare the macro's result type",
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
                        return self.eval_helper_binding(helper, name.syntax.span.clone());
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
                    "Package" => Some(VisibilityKind::Package),
                    "Public" => Some(VisibilityKind::Public),
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
                        Box::new(Value::Product(Vec::new().into(), false)),
                    ));
                }
                self.diagnostics.push(Diagnostic::new(
                    name.syntax.span.clone(),
                    format!("compile-time name `{}` is not available", name.name),
                ));
                None
            }
            Expression::Char(c) => match staple_syntax::string_literal::decode_char(&c.literal) {
                Ok(value) => Some(Value::Char(value)),
                Err(error) => {
                    self.diagnostics
                        .push(Diagnostic::new(c.syntax.span.clone(), error.to_string()));
                    None
                }
            },
            Expression::String(string) => Some(Value::String(string.literal.clone())),
            Expression::Integer(integer) => {
                integer.literal.parse::<i128>().ok().map(Value::Integer)
            }
            Expression::Float(float) => float
                .literal
                .parse::<f64>()
                .ok()
                // A float *literal* must still be finite; only computed
                // values may reify as NaN or infinity. Leaving a non-finite
                // literal unfolded lets the checker's literal rule report it.
                .filter(|value| value.is_finite())
                .map(|value| Value::Float(value, None)),
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
                        && quote.kind == staple_syntax::QuoteKind::ParseQuote
                        && let Some(expected) = sequence_element
                    {
                        self.instantiate_contextual_quote(module, quote, environment, &expected)?
                    } else if product.elements.len() == 1
                        && element.name.is_none()
                        && !element.spread
                    {
                        self.eval_expression_target(
                            module,
                            &element.value,
                            environment,
                            self.call_target.clone(),
                        )?
                    } else {
                        self.eval_expression(module, &element.value, environment)?
                    };
                    if element.spread {
                        let Value::Product(elements, _) = value else {
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
                Some(Value::Product(values.into(), product.slotted))
            }
            Expression::RepeatedProduct(repeated) => {
                let value = self.eval_expression(module, &repeated.value, environment)?;
                let count = match repeated.count.as_ref() {
                    Type::NumberLiteral(literal) => literal.literal.parse::<i128>().ok(),
                    _ => None,
                };
                let Some(count) = count else {
                    self.diagnostics.push(Diagnostic::new(
                        repeated.count.syntax().span.clone(),
                        "an array length must be a compile-time integer type",
                    ));
                    return None;
                };
                let Ok(count) = usize::try_from(count) else {
                    self.diagnostics.push(Diagnostic::new(
                        repeated.count.syntax().span.clone(),
                        "an array length must not be negative",
                    ));
                    return None;
                };
                if count > MAX_COMPILE_TIME_PRODUCT_ARITY {
                    self.diagnostics.push(Diagnostic::new(
                        repeated.syntax.span.clone(),
                        format!(
                            "compile-time product arity exceeds the limit of {MAX_COMPILE_TIME_PRODUCT_ARITY}"
                        ),
                    ));
                    return None;
                }
                Some(Value::Product(ProductValue::repeated(value, count), false))
            }
            Expression::Call(call) => {
                let (head, arguments) = flatten_call(expression);
                if let Some(result) =
                    self.eval_builtin_operator_call(module, head, &arguments, environment)
                {
                    return result;
                }
                if let Some((member, type_arguments)) =
                    self.parse_member_head(module, &call.callee, |name| {
                        environment.contains_key(name)
                    })
                {
                    return self.eval_parse_call(module, call, member, type_arguments, environment);
                }
                if let Expression::TypeApplication(application) = call.callee.as_ref() {
                    return self.eval_type_applied_call(module, call, application, environment);
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
                    // Only a `Raw` fragment needs reparsing; a structured
                    // `SyntaxValue` must be preserved as-is. Using `result.take()`
                    // as the `if let` scrutinee would drop non-`Raw` values on a
                    // failed match, so branch on the variant explicitly.
                    if matches!(result, Some(SyntaxValue::Raw(_))) {
                        let Some(SyntaxValue::Raw(raw)) = result.take() else {
                            unreachable!("checked to be a raw fragment above")
                        };
                        result = match staple_syntax::parse_expression_fragment(
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
                                expanded = Expression::Call(staple_syntax::CallExpression {
                                    spread: false,
                                    syntax,
                                    callee: Box::new(expanded),
                                    argument: Box::new((*argument).clone()),
                                });
                            }
                            result = Some(SyntaxValue::from_expression(expanded));
                        };
                    }
                    // During compile-time evaluation, a control-flow macro such
                    // as `if`/`when` expands to a `match Bool :: … { … }`
                    // over compile-time-known operands. Evaluate that expansion
                    // in place so the macro selects a branch inside compile-time
                    // helper and macro bodies, instead of yielding the `match`
                    // as an opaque syntax value the surrounding code cannot use.
                    // Only bare control-flow shapes are evaluated; a macro that
                    // builds an expression to emit (a constructor call, an
                    // identifier, a literal) is still returned as syntax, as are
                    // builtin producers (`c_string`) and excess-argument calls.
                    if arguments[consumed_count..].is_empty()
                        && matches!(definition.kind, MacroKind::User(_))
                        && let Some(expanded) = result.as_ref().and_then(SyntaxValue::to_expression)
                        && matches!(
                            expanded,
                            Expression::Match(_)
                                | Expression::TypeAscription(_)
                                | Expression::Block(_)
                                | Expression::Loop(_)
                        )
                    {
                        let expanded = expanded.clone();
                        // The expansion is done; a branch may use the same
                        // macro again.
                        self.expansion_stack.pop();
                        return self.eval_expression(module, &expanded, environment);
                    }
                    let result = result.map(Value::Syntax);
                    self.expansion_stack.pop();
                    return result;
                }
                if let Expression::Name(name) = call.callee.as_ref()
                    && name.name.chars().next().is_some_and(char::is_uppercase)
                {
                    let mut argument = self.eval_expression(module, &call.argument, environment)?;
                    if name.name == "Ok"
                        && matches!(call.argument.as_ref(), Expression::Product(product)
                            if product.elements.len() == 1
                                && product.elements[0].name.is_none()
                                && !product.elements[0].spread)
                    {
                        // `Ok (x)` wraps `x`, not a one-element product.
                        argument = ungroup_value(argument);
                    }
                    if matches!(
                        name.name.as_str(),
                        "Ident"
                            | "CallExpr"
                            | "CharExpr"
                            | "StringExpr"
                            | "BindingPattern"
                            | "NominalPattern"
                            | "ModifierCall"
                            | "UnstructuredModifier"
                            | "ModifiedItem"
                            | "Sequence"
                            | "Separated"
                            | "Parenthesized"
                            | "Bracketed"
                            | "Angled"
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
                let wants_place = self.callable_takes_mut(&callee);
                let (argument, place) =
                    self.eval_call_argument(module, &call.argument, environment, wants_place)?;
                self.argument_place = place;
                let fills_slots = self
                    .slotted_argument_fills
                    .get(&call.argument.syntax().id)
                    .copied();
                let target = self
                    .call_targets
                    .get(&call.syntax.id)
                    .and_then(|ty| parse::compile_type_target(ty, &self.type_env))
                    .or_else(|| self.call_target.clone());
                let previous = std::mem::replace(&mut self.call_target, target);
                let result = self.apply_value_with_slots(
                    callee,
                    argument,
                    call.syntax.span.clone(),
                    fills_slots,
                );
                self.call_target = previous;
                result
            }
            Expression::Access(access) => {
                if let Accessor::Method(name) = &access.accessor {
                    return Some(Value::Method(module, name.clone()));
                }
                if let Some(helper) = self.qualified_compile_helper(module, access)
                    && helper.binding.visibility == Visibility::Public
                {
                    return self.eval_helper_binding(helper, access.syntax.span.clone());
                }
                if let (Expression::Name(ty), Accessor::Name(member)) =
                    (access.value.as_ref(), &access.accessor)
                    && ty.name.chars().next().is_some_and(char::is_uppercase)
                    && !environment.contains_key(&ty.name)
                {
                    return Some(Value::Method(module, member.clone()));
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
                if let Value::Separated {
                    elements,
                    separator,
                    trailing,
                } = &value
                    && let Accessor::Name(name) = &access.accessor
                {
                    return match name.as_str() {
                        "elements" => Some(Value::Sequence(elements.clone().into())),
                        "separator" => Some(separator.as_ref().clone()),
                        "trailing" => Some(bool_value(*trailing)),
                        _ => {
                            self.diagnostics.push(Diagnostic::new(
                                access.syntax.span.clone(),
                                format!("separated syntax has no field `{name}`"),
                            ));
                            None
                        }
                    };
                }
                if let Value::Syntax(SyntaxValue::Modifier(modifier)) = &value {
                    if modifier.invocation.doc.is_some() {
                        self.diagnostics.push(Diagnostic::new(
                            access.syntax.span.clone(),
                            "unstructured modifier syntax has no inspectable fields",
                        ));
                        return None;
                    }
                    value = modifier_call_value(&modifier.invocation);
                }
                if let Value::Syntax(SyntaxValue::Parameters { fields, .. }) = &value {
                    value = Value::Product(fields.clone().into(), false);
                }
                if let Value::Syntax(SyntaxValue::Item(item)) = &value {
                    if let Item::TypeDeclaration(declaration) = item.as_ref() {
                        value = type_declaration_item_value(declaration);
                    } else if let Item::Modified(modified) = item.as_ref() {
                        value = modified_item_value(modified);
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
                let Value::Product(elements, _) = value else {
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
                let Value::Product(elements, _) = value else {
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
                // A parenthesized subject also matches as its single element.
                let ungrouped = ungroup_value(subject.clone());
                for candidate in [&ungrouped, &subject] {
                    for arm in &match_.arms {
                        let mut bindings = Environment::new();
                        if match_pattern(&arm.pattern, candidate, &mut bindings) {
                            let mut arm_environment = environment.clone();
                            arm_environment.extend(bindings);
                            return self.eval_expression(module, &arm.body, &mut arm_environment);
                        }
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
                let body = Expression::Block(loop_.body.clone());
                loop {
                    let result = self.eval_expression(module, &body, environment);
                    if self.pending_return.is_some() {
                        return None;
                    }
                    if let Some(value) = self.pending_break.take() {
                        return Some(value);
                    }
                    if std::mem::take(&mut self.pending_continue) {
                        continue;
                    }
                    result?;
                }
            }
            Expression::Coro(coro) => {
                self.diagnostics.push(Diagnostic::new(
                    coro.syntax.span.clone(),
                    "coroutines are not available during compile-time macro evaluation",
                ));
                None
            }
            Expression::Await(await_) => {
                self.diagnostics.push(Diagnostic::new(
                    await_.syntax.span.clone(),
                    "`await` is not available during compile-time macro evaluation",
                ));
                None
            }
            Expression::Try(try_) => {
                // `Ok x` is `x`; anything else leaves the innermost
                // compile-time function with that value.
                match self.eval_expression(module, &try_.operand, environment)? {
                    Value::Nominal(name, payload) if name == "Ok" => Some(*payload),
                    residual => {
                        self.pending_return = Some(residual);
                        None
                    }
                }
            }
            Expression::Conversion(conversion) => {
                self.diagnostics.push(Diagnostic::new(
                    conversion.syntax.span.clone(),
                    "`as` is not available during compile-time macro evaluation",
                ));
                None
            }
            Expression::Pipe(pipe) => {
                let lowered = lower_pipe_expression(pipe.clone(), &mut self.next_syntax_id);
                self.eval_expression_target(module, &lowered, environment, self.call_target.clone())
            }
            Expression::Block(block) => {
                let mut local = environment.clone();
                let mut result = Value::Product(Vec::new().into(), false);
                for (index, item) in block.items.iter().enumerate() {
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
                                binding_signature(binding).as_ref().and_then(meta_type)
                                && let Expression::Quote(quote) = value
                                && quote.kind == staple_syntax::QuoteKind::ParseQuote
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
                                let target = binding_signature(binding)
                                    .as_ref()
                                    .and_then(|ty| self.resolve_type(ty));
                                self.eval_expression_target(module, value, &mut local, target)?
                            };
                            local.insert(
                                binding.name.clone(),
                                EnvironmentBinding::new(value, binding.mutable),
                            );
                        }
                        Item::PatternBinding(binding) => {
                            let value = self.eval_expression(module, &binding.value, &mut local)?;
                            if !match_pattern(&binding.pattern, &value, &mut local) {
                                self.diagnostics.push(Diagnostic::new(
                                    binding.pattern.syntax().span.clone(),
                                    "this compile-time value does not match the pattern",
                                ));
                                return None;
                            }
                        }
                        Item::Assignment(assignment) => {
                            let value = if let Expression::Quote(quote) = &assignment.value
                                && quote.kind == staple_syntax::QuoteKind::ParseQuote
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
                            result = if index + 1 == block.items.len() {
                                self.eval_expression_target(
                                    module,
                                    value,
                                    &mut local,
                                    self.call_target.clone(),
                                )?
                            } else {
                                self.eval_expression(module, value, &mut local)?
                            };
                        }
                        Item::Return(return_) => {
                            // `return` leaves the whole function, not just
                            // this block.
                            let value = self.eval_expression(module, &return_.value, &mut local)?;
                            self.pending_return = Some(value);
                            return None;
                        }
                        Item::Break(break_) => {
                            let value = match &break_.value {
                                Some(value) => self.eval_expression(module, value, &mut local)?,
                                None => Value::Product(Vec::new().into(), false),
                            };
                            self.pending_break = Some(value);
                            return None;
                        }
                        Item::Continue(_) => {
                            self.pending_continue = true;
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
                let spelling = staple_syntax::string_literal::decode(&literal).unwrap_or(literal);
                let tokens = staple_syntax::lex(&spelling)
                    .into_iter()
                    .filter(|token| !token.kind.is_trivia())
                    .collect::<Vec<_>>();
                if !matches!(tokens.as_slice(), [token]
                    if token.kind == staple_syntax::TokenKind::Identifier && token.text == spelling)
                {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        format!("`{spelling}` is not a valid identifier spelling"),
                    ));
                    return None;
                }
                let syntax = self.generated_syntax(module, span);
                Some(Value::Syntax(SyntaxValue::Ident(
                    staple_syntax::NameExpression {
                        syntax,
                        name: spelling,
                    },
                )))
            }
            "CallExpr" => {
                let Value::Product(elements, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`CallExpr` requires `(callee: Expr, argument: Expr)`",
                    ));
                    return None;
                };
                let elements = elements.to_vec();
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
                Some(Value::Syntax(SyntaxValue::Call(
                    staple_syntax::CallExpression {
                        spread: false,
                        syntax,
                        callee: Box::new(callee.to_expression()?),
                        argument: Box::new(argument.to_expression()?),
                    },
                )))
            }
            "CharExpr" => {
                let Value::Char(value) = argument else {
                    self.diagnostics
                        .push(Diagnostic::new(span, "`CharExpr` requires a Char"));
                    return None;
                };
                let syntax = self.generated_syntax(module, span);
                Some(Value::Syntax(SyntaxValue::Unstructured(Expression::Char(
                    staple_syntax::CharExpression {
                        syntax,
                        literal: staple_syntax::string_literal::encode_char(value),
                    },
                ))))
            }
            "StringExpr" => {
                let Value::String(value) = argument else {
                    self.diagnostics
                        .push(Diagnostic::new(span, "`StringExpr` requires a string"));
                    return None;
                };
                let syntax = self.generated_syntax(module, span);
                Some(Value::Syntax(SyntaxValue::Unstructured(
                    Expression::String(staple_syntax::StringExpression {
                        syntax,
                        literal: staple_syntax::string_literal::encode(&value),
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
                    staple_syntax::BindingPattern {
                        syntax: identifier.syntax,
                        mutable: false,
                        moved: false,
                        name: identifier.name.clone(),
                        resolution_name: Some(identifier.name),
                        ty: Type::Inferred(staple_syntax::InferredType::new()),
                    },
                ))))
            }
            "NominalPattern" => {
                let Value::Product(fields, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`NominalPattern` requires `(name: Ident<String>, argument: Pattern)`",
                    ));
                    return None;
                };
                let fields = fields.to_vec();
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
                    staple_syntax::NominalPattern {
                        syntax: self.generated_syntax(module, span),
                        namespace: None,
                        name: name.name.clone(),
                        moved: false,
                        argument: Box::new(argument.clone()),
                        annotation: None,
                    },
                ))))
            }
            "ModifierCall" => {
                let invalid = "`ModifierCall` requires `(namespace: Sequence<Ident<String>>, name: Ident<String>, argument: Option<Syntax>)`";
                let Value::Product(fields, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(span, invalid));
                    return None;
                };
                let fields = fields.to_vec();
                let [
                    (Some(namespace_field), Value::Sequence(namespace)),
                    (Some(name_field), Value::Syntax(SyntaxValue::Ident(name))),
                    (Some(argument_field), argument),
                ] = fields.as_slice() else {
                    self.diagnostics.push(Diagnostic::new(span, invalid));
                    return None;
                };
                if namespace_field != "namespace" || name_field != "name" || argument_field != "argument" {
                    self.diagnostics.push(Diagnostic::new(span, invalid));
                    return None;
                }
                let Some(namespace) = namespace.iter().map(|value| match value {
                    Value::Syntax(SyntaxValue::Ident(name)) => Some(name.name.clone()),
                    _ => None,
                }).collect::<Option<Vec<_>>>() else {
                    self.diagnostics.push(Diagnostic::new(span, invalid));
                    return None;
                };
                let path = namespace.iter().map(|name| format!("{name}.")).collect::<String>();
                let mut tokens = staple_syntax::lex(&format!("@{path}{}", name.name));
                match argument {
                    Value::Nominal(kind, _) if kind == "None" => {},
                    Value::Nominal(kind, value) if kind == "Some" => {
                        // `quote` may return a structured value, so accept any
                        // syntax value and splice its tokens.
                        let Some(syntax) = parse::value_syntax(value.as_ref()) else {
                            self.diagnostics.push(Diagnostic::new(span, invalid));
                            return None;
                        };
                        tokens.extend(staple_syntax::lex("("));
                        append_syntax_tokens(&mut tokens, &syntax);
                        tokens.extend(staple_syntax::lex(")"));
                    },
                    _ => {
                        self.diagnostics.push(Diagnostic::new(span, invalid));
                        return None;
                    },
                }
                let mut offset = 0;
                for token in &mut tokens {
                    token.span = offset..offset + token.text.len();
                    offset += token.text.len();
                }
                let mut syntax = self.generated_syntax(module, span);
                syntax.definition_module = name.syntax.definition_module;
                syntax.expansion_mark = name.syntax.expansion_mark;
                syntax.tokens = tokens.into();
                syntax.token_range = 0..syntax.tokens.len();
                let invocation = match staple_syntax::parse_modifier_fragment(&syntax, &mut self.next_syntax_id) {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        self.diagnostics.push(Diagnostic::new(syntax.span, format!("invalid `ModifierCall`: {}", error.message)));
                        return None;
                    },
                };
                Some(Value::Syntax(SyntaxValue::Modifier(OpaqueModifier { invocation, enclosing: None })))
            }
            "ModifiedItem" => {
                let Value::Product(fields, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`ModifiedItem` requires `(modifiers: Sequence<Modifier>, item: Item)`",
                    ));
                    return None;
                };
                let fields = fields.to_vec();
                let [
                    (Some(modifiers_name), Value::Sequence(modifiers)),
                    (Some(item_name), Value::Syntax(SyntaxValue::Item(item))),
                ] = fields.as_slice()
                else {
                    self.diagnostics
                        .push(Diagnostic::new(span, "invalid `ModifiedItem` fields"));
                    return None;
                };
                if modifiers_name != "modifiers" || item_name != "item" {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`ModifiedItem` fields must be named `modifiers` and `item`",
                    ));
                    return None;
                }
                let enclosing = modifiers.first().and_then(|value| match value {
                    Value::Syntax(SyntaxValue::Modifier(modifier)) => modifier.enclosing.clone(),
                    _ => None,
                });
                let modifiers = modifiers
                    .iter()
                    .map(|value| match value {
                        Value::Syntax(SyntaxValue::Modifier(modifier)) => {
                            Some(modifier.invocation.clone())
                        }
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>();
                let Some(modifiers) = modifiers else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`ModifiedItem.modifiers` must contain only `Modifier` values",
                    ));
                    return None;
                };
                if modifiers.is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`ModifiedItem.modifiers` must not be empty",
                    ));
                    return None;
                }
                let syntax = enclosing.unwrap_or_else(|| self.generated_syntax(module, span));
                Some(Value::Syntax(SyntaxValue::Item(Box::new(Item::Modified(
                    staple_syntax::ModifiedItem {
                        syntax,
                        modifiers,
                        item: item.clone(),
                    },
                )))))
            }
            "Sequence" => {
                let Value::Product(elements, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`Sequence` requires a product of syntax values",
                    ));
                    return None;
                };
                let elements = elements.to_vec();
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
                    return Some(Value::Sequence(values.into()));
                }
                Some(Value::Sequence(
                    elements.into_iter().map(|(_, value)| value).collect(),
                ))
            }
            "Separated" => {
                let Value::Product(fields, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`Separated` requires `(separator: Syntax, elements: (...), trailing: Bool)`",
                    ));
                    return None;
                };
                let fields = fields.to_vec();
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
                    Value::Product(elements, _) => {
                        elements.iter().map(|(_, value)| value.clone()).collect()
                    }
                    Value::Sequence(elements) => elements.to_vec(),
                    _ => {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            "`Separated.elements` must be a product or `Sequence`",
                        ));
                        return None;
                    }
                };
                let trailing = match trailing {
                    Value::Nominal(name, value) if matches!(value.as_ref(), Value::Product(values, _) if values.is_empty()) => {
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
            "Parenthesized" | "Bracketed" | "Braced" | "Angled" => {
                let kind = delimiter_kind(constructor).expect("matched delimiter constructor");
                let contents = match argument {
                    Value::Syntax(SyntaxValue::Raw(syntax)) => DelimitedValueContents::Slotted(
                        vec![(None, Value::Syntax(SyntaxValue::Raw(syntax)))],
                    ),
                    Value::Product(mut values, _)
                        if values.len() == 1 && matches!(values[0].1, Value::Sequence(_)) =>
                    {
                        let (_, Value::Sequence(values)) = values.remove(0) else {
                            unreachable!("guarded sequence contents")
                        };
                        DelimitedValueContents::Sequence(values.into())
                    }
                    Value::Product(mut values, _)
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
                    Value::Product(values, slotted) => {
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
                        // Slotted contents are juxtaposed; product contents
                        // are separated by commas.
                        if slotted || values.len() < 2 {
                            DelimitedValueContents::Slotted(values.to_vec())
                        } else {
                            DelimitedValueContents::Product(values.to_vec())
                        }
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
                    DelimitedValueContents::Slotted(values)
                    | DelimitedValueContents::Product(values) => values.len(),
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
            Value::Product(mut elements, slotted) => {
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
                Some(Value::Product(elements, slotted))
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
        match result {
            None => self.pending_return.take(),
            result => result,
        }
    }

    /// Evaluates a named helper: an intrinsic or a function stays a callable
    /// value, and any other binding is evaluated from its initializer. A
    /// binding annotated with a float type gives its value that type, so
    /// `F32.Pi` is an `F32` wherever it is read.
    fn eval_helper_binding(&mut self, helper: HelperDefinition, span: Span) -> Option<Value> {
        if let Some(intrinsic) = self.helper_intrinsic(&helper) {
            return Some(Value::Intrinsic(intrinsic));
        }
        if helper.binding.value.is_none() && helper.binding.kind == BindingKind::Extern {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "`{}` is a runtime-only staple-intrinsic with no compile-time implementation",
                    helper.binding.name
                ),
            ));
            return None;
        }
        if matches!(helper.binding.value, Some(Expression::Function(_))) {
            return Some(Value::Helper(helper.module, helper.binding));
        }
        let float = helper
            .binding
            .annotation_body()
            .and_then(|annotation| self.named_float_type(helper.module, annotation));
        let value = helper.binding.value?;
        let value = self.eval_helper_value(helper.module, &value, span.clone())?;
        self.assign_float_type(value, float, span)
    }

    /// The `companion F32`/`companion F64` module of `float`.
    fn float_companion(&self, float: FloatType) -> Option<ModuleId> {
        match float {
            FloatType::F32 => self.float_companion_modules[0],
            FloatType::F64 => self.float_companion_modules[1],
        }
    }

    /// The float type a type expression names in `module`. A type name maps
    /// to its companion's namespace, so this recognizes `F64` and an import
    /// of it under another name, and not an unrelated type called `F64`.
    fn named_float_type(&self, module: ModuleId, ty: &Type) -> Option<FloatType> {
        let Type::Named(named) = ty else {
            return None;
        };
        if named.namespace.is_some() {
            return None;
        }
        let mut cursor = module;
        loop {
            if let Some(target) = self.scopes[cursor.0].namespaces.get(&named.name) {
                return FloatType::ALL
                    .into_iter()
                    .find(|float| self.float_namespaces[*float as usize] == Some(*target));
            }
            if !self.companion_modules[cursor.0] {
                return None;
            }
            cursor = self.parent_modules[cursor.0]?;
        }
    }

    /// Gives an untyped float the float type its context expects, rounding it
    /// to that precision. A float already known to have a different type is
    /// a type error: the folded constant replaces its initializer, so the
    /// checker would never see the mismatch.
    fn assign_float_type(
        &mut self,
        value: Value,
        float: Option<FloatType>,
        span: Span,
    ) -> Option<Value> {
        match (value, float) {
            (Value::Float(value, None), Some(float)) => {
                Some(Value::Float(parse::round_float(float, value), Some(float)))
            }
            (Value::Float(_, Some(actual)), Some(float)) if actual != float => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("expected `{}`, found `{}`", float.name(), actual.name()),
                ));
                None
            }
            (value, _) => Some(value),
        }
    }

    /// Folds an arithmetic or comparison operator over two floats. An untyped
    /// operand takes the other operand's type, and the arithmetic happens at
    /// that type's precision, as it would at run time.
    fn fold_float_operator(
        &mut self,
        operator: &str,
        (left, left_type): (f64, Option<FloatType>),
        (right, right_type): (f64, Option<FloatType>),
        span: Span,
    ) -> Option<Value> {
        use crate::FloatBinaryOperation as Operation;
        let float = match (left_type, right_type) {
            (Some(left), Some(right)) if left != right => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "operator `{operator}` cannot combine `{}` and `{}`",
                        left.name(),
                        right.name()
                    ),
                ));
                return None;
            }
            (left, right) => left.or(right),
        };
        let precision = float.unwrap_or(FloatType::F64);
        let left = parse::round_float(precision, left);
        let right = parse::round_float(precision, right);
        let operation = match operator {
            "+" => Operation::Add,
            "-" => Operation::Subtract,
            "*" => Operation::Multiply,
            "/" => Operation::Divide,
            "%" => Operation::Remainder,
            "==" => return Some(bool_value(left == right)),
            "!=" => return Some(bool_value(left != right)),
            "<" => return Some(bool_value(left < right)),
            "<=" => return Some(bool_value(left <= right)),
            ">" => return Some(bool_value(left > right)),
            ">=" => return Some(bool_value(left >= right)),
            _ => unreachable!("not a builtin operator: {operator}"),
        };
        Some(Value::Float(
            parse::apply_float_binary(precision, operation, left, right),
            float,
        ))
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
        self.apply_value_with_slots(callee, argument, span, None)
    }

    /// Applies a callable while carrying the static checker's decision about
    /// whether `argument` fills every remaining slot of a slotted parameter.
    /// `None` means the application was not statically checked (for example a
    /// call produced by macro expansion), so evaluation falls back to the
    /// argument's shape.
    fn apply_value_with_slots(
        &mut self,
        callee: Value,
        argument: Value,
        span: Span,
        fills_slots: Option<bool>,
    ) -> Option<Value> {
        if self.helper_eval_depth >= MAX_HELPER_EVAL_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                span,
                "compile-time evaluation recursed too deeply; check for a self-referential binding",
            ));
            self.argument_place = None;
            return None;
        }
        self.helper_eval_depth += 1;
        let result = self.apply_value_inner(callee, argument, span, fills_slots);
        self.helper_eval_depth -= 1;
        result
    }

    fn apply_value_inner(
        &mut self,
        callee: Value,
        argument: Value,
        span: Span,
        fills_slots: Option<bool>,
    ) -> Option<Value> {
        let place = self.argument_place.take();
        match callee {
            Value::Method(module, name) => {
                let callee = self.resolve_method(module, &name, &argument, span.clone())?;
                self.apply_value_inner(callee, argument, span, fills_slots)
            }
            Value::TraitMember(key, member, explicit, pending) => {
                self.argument_place = place;
                self.apply_trait_member(
                    key,
                    member,
                    argument,
                    self.call_target.clone(),
                    explicit,
                    pending,
                    span,
                )
            }
            Value::TypeApplied(callee, arguments) => match *callee {
                Value::TraitMember(key, member, _, pending) => {
                    self.argument_place = place;
                    self.apply_trait_member(
                        key,
                        member,
                        argument,
                        self.call_target.clone(),
                        arguments,
                        pending,
                        span,
                    )
                }
                Value::Method(module, name) => {
                    let mut value = self.resolve_method(module, &name, &argument, span.clone())?;
                    self.bind_type_arguments(&mut value, &arguments);
                    self.apply_value_inner(value, argument, span, fills_slots)
                }
                Value::Helper(module, binding) => {
                    let mut value =
                        self.eval_helper_value(module, binding.value.as_ref()?, span.clone())?;
                    self.bind_type_arguments(&mut value, &arguments);
                    self.apply_value_inner(value, argument, span, fills_slots)
                }
                mut value => {
                    self.bind_type_arguments(&mut value, &arguments);
                    if let Value::Nominal(name, _) = value {
                        if matches!(
                            name.as_str(),
                            "Ident"
                                | "CallExpr"
                                | "CharExpr"
                                | "StringExpr"
                                | "BindingPattern"
                                | "NominalPattern"
                                | "ModifierCall"
                                | "UnstructuredModifier"
                                | "ModifiedItem"
                                | "Sequence"
                                | "Separated"
                                | "Parenthesized"
                                | "Bracketed"
                                | "Angled"
                                | "Braced"
                        ) {
                            return self.construct_syntax(ModuleId(0), &name, argument, span);
                        }
                        return Some(Value::Nominal(name, Box::new(argument)));
                    }
                    self.apply_value_inner(value, argument, span, fills_slots)
                }
            },
            Value::Function {
                module,
                mut function,
                mut environment,
                quote_result,
                result_target,
                mut type_env,
            } => {
                self.bind_function_types(&function, &argument, &mut type_env);
                // A slotted function is curried by juxtaposed calls: after
                // `F64.min a`, the remaining `b` is still awaited. When the
                // static checker saw one argument fill every slot, bind the
                // whole product; otherwise use the argument's shape.
                let whole_product = matches!(
                    (&argument, &function.pattern),
                    (Value::Product(fields, true), Pattern::Product(product))
                        if product.slotted && product.elements.len() == fields.len()
                ) && fills_slots.unwrap_or(true);
                if !whole_product
                    && let Pattern::Product(product) = &mut function.pattern
                    && product.slotted
                    && product.elements.len() > 1
                {
                    let mut elements = std::mem::take(&mut product.elements);
                    let first = elements.remove(0);
                    let rest = if elements.len() == 1 {
                        elements.remove(0)
                    } else {
                        Pattern::Product(staple_syntax::ProductPattern {
                            slotted: true,
                            syntax: product.syntax.clone(),
                            elements,
                            mutable: false,
                            moved: false,
                            annotation: None,
                        })
                    };
                    if !bind_pattern(&first, argument, &mut environment) {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            "compile-time function argument does not match its parameter",
                        ));
                        return None;
                    }
                    return Some(Value::Function {
                        module,
                        function: staple_syntax::FunctionExpression {
                            syntax: function.syntax,
                            generics: None,
                            pattern: rest,
                            effect_lower_bound: None,
                            body: function.body,
                        },
                        environment,
                        quote_result,
                        result_target,
                        type_env,
                    });
                }
                if !bind_pattern(&function.pattern, argument, &mut environment) {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "compile-time function argument does not match its parameter",
                    ));
                    return None;
                }
                // A `mut` parameter is the caller's own storage, not a copy.
                if let Some(place) = place
                    && let Some(name) = mutable_parameter_name(&function.pattern)
                {
                    environment.insert(name.to_owned(), place);
                }
                let previous_context = match quote_result {
                    Some(expected) => self.quote_context.replace(expected),
                    None => self.quote_context.take(),
                };
                let previous_env = std::mem::replace(&mut self.type_env, type_env);
                let result = match result_target {
                    Some(target) => self.eval_expression_target(
                        module,
                        &function.body,
                        &mut environment,
                        Some(target),
                    ),
                    None => self.eval_expression(module, &function.body, &mut environment),
                };
                self.type_env = previous_env;
                self.quote_context = previous_context;
                // A `?` or `return` leaves this function with its value.
                match result {
                    None => self.pending_return.take(),
                    result => result,
                }
            }
            Value::Intrinsic(intrinsic) => {
                self.apply_intrinsic(intrinsic, None, argument, place, span)
            }
            Value::Helper(module, binding) => {
                if Some(module) == self.standard_library_process && binding.name == "panic" {
                    let message = match &argument {
                        Value::String(message) => staple_syntax::string_literal::decode(message)
                            .unwrap_or_else(|_| message.clone()),
                        _ => "compile-time panic".to_string(),
                    };
                    let description = match self.expansion_stack.last() {
                        Some(key) => format!("macro `{}` panicked: {message}", key.name),
                        None => format!("panicked at compile time: {message}"),
                    };
                    let report_span = self.invocation_spans.last().cloned().unwrap_or(span);
                    self.diagnostics
                        .push(Diagnostic::new(report_span, description));
                    self.suppress_expansion_trailer = true;
                    return None;
                }
                let expected = binding_signature(&binding)
                    .as_ref()
                    .and_then(function_result_meta_type);
                let value = binding.value?;
                let previous_context = expected
                    .map(|expected| self.quote_context.replace(expected))
                    .unwrap_or_else(|| self.quote_context.take());
                let function = self.eval_helper_value(module, &value, span.clone());
                self.quote_context = previous_context;
                // Unwrapping a helper into the function it names and then
                // applying that function is one logical call, not two — go
                // through `apply_value_inner` directly so `helper_eval_depth`
                // (and the resulting recursion-depth budget) tracks actual
                // call nesting rather than this implementation detail.
                self.argument_place = place;
                self.apply_value_inner(function?, argument, span, fills_slots)
            }
            _ => {
                self.diagnostics
                    .push(Diagnostic::new(span, "cannot call this compile-time value"));
                None
            }
        }
    }

    fn eval_binary_expression(
        &mut self,
        module: ModuleId,
        binary: &staple_syntax::BinaryExpression,
        environment: &mut Environment,
    ) -> Option<Value> {
        let lowered = lower_binary_expression(
            binary.clone(),
            &mut self.next_syntax_id,
            StdModules::default(),
        );
        self.eval_expression(module, &lowered, environment)
    }

    /// Folds a prefix operator (`-x`, `!x`) over a compile-time value. The
    /// standard library backs `Neg`/`Not` with ordinary Staple code rather
    /// than intrinsics, so — as with `eval_builtin_operator_call` for the
    /// binary operators — the fold is done directly here on the known scalar
    /// representations.
    fn eval_unary_expression(
        &mut self,
        module: ModuleId,
        unary: &staple_syntax::UnaryExpression,
        environment: &mut Environment,
    ) -> Option<Value> {
        let operand = self.eval_expression(module, &unary.operand, environment)?;
        match (unary.operator, &operand) {
            (staple_syntax::UnaryOperator::Negate, Value::Integer(value)) => {
                Some(Value::Integer(value.wrapping_neg()))
            }
            (staple_syntax::UnaryOperator::Negate, Value::Float(value, float)) => {
                Some(Value::Float(-value, *float))
            }
            (staple_syntax::UnaryOperator::Not, Value::Nominal(name, _))
                if name == "True" || name == "False" =>
            {
                Some(bool_value(name == "False"))
            }
            _ => {
                self.diagnostics.push(Diagnostic::new(
                    unary.operator_syntax.span.clone(),
                    format!(
                        "operator `{}` is not available for this compile-time value",
                        unary.operator.text()
                    ),
                ));
                None
            }
        }
    }

    /// Converts a compile-time `Value` produced by folding a `const`
    /// initializer back into a literal `Expression` the rest of the
    /// pipeline (resolve, typecheck, codegen) can treat like any other
    /// literal-valued binding. Integers, floats, strings, and (possibly
    /// nested) products of those can be represented this way. `expected` is
    /// the binding's annotation, read in `module`, which lets an untyped
    /// float, including one inside a product, take the float type it names.
    fn value_to_expression(
        &mut self,
        value: Value,
        span: Span,
        module: ModuleId,
        expected: Option<&Type>,
    ) -> Option<Expression> {
        let float_hint = expected.and_then(|expected| self.named_float_type(module, expected));
        let value = match value {
            Value::Float(float, None) if float_hint.is_some() => Value::Float(float, float_hint),
            value => value,
        };
        match value {
            Value::Integer(integer) if integer >= 0 => {
                Some(Expression::Integer(staple_syntax::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span),
                    literal: integer.to_string(),
                }))
            }
            Value::Integer(integer) => {
                // Literals cannot carry a leading `-`. Keep `0 - |n|` as
                // operator syntax so desugaring binds the standard trait.
                let zero = Expression::Integer(staple_syntax::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: "0".to_string(),
                });
                let magnitude = Expression::Integer(staple_syntax::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: integer.unsigned_abs().to_string(),
                });
                Some(Expression::Binary(staple_syntax::BinaryExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    operator_syntax: Syntax::synthetic(self.fresh_id(), span),
                    operator: BinaryOperator::Subtract,
                    left: Box::new(zero),
                    right: Box::new(magnitude),
                }))
            }
            // A literal takes its type from context, so it spells an untyped
            // float, or a typed one where the binding expects that type. A
            // reified literal may carry a leading `-`: source literals cannot,
            // but this is compiler-built syntax that later phases only parse.
            Value::Float(float, float_type)
                if float.is_finite() && (float_type.is_none() || float_type == float_hint) =>
            {
                Some(self.float_literal(float, span))
            }
            // An untyped NaN or infinity came from literal arithmetic, and
            // reifies as that arithmetic so its type is still inferred.
            Value::Float(float, None) => {
                let numerator = if float.is_nan() { 0.0 } else { float.signum() };
                let numerator = self.float_literal(numerator, span.clone());
                let zero = self.float_literal(0.0, span.clone());
                Some(Expression::Binary(staple_syntax::BinaryExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    operator_syntax: Syntax::synthetic(self.fresh_id(), span),
                    operator: BinaryOperator::Divide,
                    left: Box::new(numerator),
                    right: Box::new(zero),
                }))
            }
            // Any other typed float, including a non-finite one, reifies as
            // its exact bit pattern through its type's `from_bits`, which
            // fixes the type and keeps NaN payload bits.
            Value::Float(float, Some(float_type)) => {
                let Some(companion) = self.float_companion(float_type) else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "a typed compile-time float requires the standard library's float companions",
                    ));
                    return None;
                };
                let bits = match float_type {
                    FloatType::F32 => u64::from(parse::narrow_to_f32(float).to_bits()),
                    FloatType::F64 => float.to_bits(),
                };
                let callee = Expression::Name(staple_syntax::NameExpression {
                    syntax: {
                        let mut syntax = Syntax::synthetic(self.fresh_id(), span.clone());
                        syntax.definition_module = Some(companion.0);
                        syntax
                    },
                    name: format!("__{}_from_bits", float_type.intrinsic_name()),
                });
                let argument = Expression::Integer(staple_syntax::IntegerExpression {
                    syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                    literal: bits.to_string(),
                });
                Some(Expression::Call(staple_syntax::CallExpression {
                    spread: false,
                    syntax: Syntax::synthetic(self.fresh_id(), span),
                    callee: Box::new(callee),
                    argument: Box::new(argument),
                }))
            }
            Value::Char(value) => Some(Expression::Char(staple_syntax::CharExpression {
                syntax: Syntax::synthetic(self.fresh_id(), span),
                literal: staple_syntax::string_literal::encode_char(value),
            })),
            // `Value::String`'s payload is already the raw, quoted source
            // literal (see `Expression::String`'s handling in
            // `eval_expression`, which clones `StringExpression::literal`
            // verbatim rather than decoding it) — reuse it as-is rather
            // than re-encoding, which would double-quote the result.
            Value::String(string) => Some(Expression::String(staple_syntax::StringExpression {
                syntax: Syntax::synthetic(self.fresh_id(), span),
                literal: string,
            })),
            Value::Product(fields, slotted) => {
                let element_types: Vec<Option<&Type>> = match expected {
                    Some(Type::Product(product))
                        if !product.variadic && product.elements.len() == fields.len() =>
                    {
                        product
                            .elements
                            .iter()
                            .map(|element| Some(&element.ty))
                            .collect()
                    }
                    _ => vec![None; fields.len()],
                };
                let elements = fields
                    .into_iter()
                    .zip(element_types)
                    .map(|((name, value), expected)| {
                        Some(staple_syntax::ProductElement {
                            syntax: Syntax::synthetic(self.fresh_id(), span.clone()),
                            name,
                            designated: false,
                            value: self.value_to_expression(
                                value,
                                span.clone(),
                                module,
                                expected,
                            )?,
                            spread: false,
                            named_spread: false,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(Expression::Product(staple_syntax::ProductExpression {
                    slotted,
                    syntax: Syntax::synthetic(self.fresh_id(), span),
                    elements,
                }))
            }
            other => {
                let description = match other {
                    Value::Function { .. }
                    | Value::Helper(..)
                    | Value::Intrinsic(_)
                    | Value::TraitMember(..)
                    | Value::Method(..)
                    | Value::TypeApplied(..) => "a function value",
                    Value::Stream { .. } => "a parse stream",
                    Value::Syntax(_) => "a syntax value",
                    Value::Sequence(_) => "a sequence value",
                    Value::Separated { .. } => "a separated sequence value",
                    Value::Nominal(..) => "a nominal value",
                    Value::Integer(_)
                    | Value::Float(..)
                    | Value::Char(_)
                    | Value::String(_)
                    | Value::Product(_, _) => {
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

    fn float_literal(&mut self, float: f64, span: Span) -> Expression {
        Expression::Float(staple_syntax::FloatExpression {
            syntax: Syntax::synthetic(self.fresh_id(), span),
            literal: format!("{float:?}"),
        })
    }

    /// Folds a lowered builtin arithmetic/comparison operator call (e.g.
    /// `Add.add (left; right)` for `left + right`, or
    /// `PartialOrd.lt (left; right)` for `left < right`), the same way the
    /// language's former `infix` expression evaluator once folded operator
    /// chains directly by operator name. Returns `None` when `head`/`arguments`
    /// doesn't match one of these ten known trait/method pairs with exactly
    /// two operands, so the caller falls back to ordinary call evaluation
    /// (this also means `..`/`..=`, which lower to plain function calls
    /// rather than a trait method, are left to that fallback and remain
    /// unsupported at compile time, unchanged from before).
    fn eval_builtin_operator_call(
        &mut self,
        module: ModuleId,
        head: &Expression,
        arguments: &[&Expression],
        environment: &mut Environment,
    ) -> Option<Option<Value>> {
        const BUILTIN_OPERATOR_METHODS: [(&str, &str, &str); 11] = [
            ("Add", "add", "+"),
            ("Subtract", "subtract", "-"),
            ("Multiply", "multiply", "*"),
            ("Divide", "divide", "/"),
            ("Remainder", "remainder", "%"),
            ("Eq", "eq", "=="),
            ("Eq", "ne", "!="),
            ("PartialOrd", "lt", "<"),
            ("PartialOrd", "le", "<="),
            ("PartialOrd", "gt", ">"),
            ("PartialOrd", "ge", ">="),
        ];
        // Lowering rewrites `left <op> right` to `Trait.method (left; right)`:
        // a single call whose argument is a two-element slotted product. A
        // directly written juxtaposed call (`Trait.method left right`)
        // supplies one slot per argument instead.
        let (left_argument, right_argument) = match arguments {
            [left, right] => (*left, *right),
            [argument] => {
                let Expression::Product(product) = argument else {
                    return None;
                };
                if product.elements.len() != 2
                    || product.elements.iter().any(|element| {
                        element.spread || element.named_spread || element.name.is_some()
                    })
                {
                    return None;
                }
                (&product.elements[0].value, &product.elements[1].value)
            }
            _ => return None,
        };
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
        let Some(left) = self.eval_expression(module, left_argument, environment) else {
            return Some(None);
        };
        let Some(right) = self.eval_expression(module, right_argument, environment) else {
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
            (Value::Integer(left), Value::Integer(right), "%") if *right != 0 => {
                Some(Value::Integer(left.wrapping_rem(*right)))
            }
            (Value::Integer(left), Value::Integer(right), "==") => Some(bool_value(left == right)),
            (Value::Integer(left), Value::Integer(right), "!=") => Some(bool_value(left != right)),
            (Value::Integer(left), Value::Integer(right), "<") => Some(bool_value(left < right)),
            (Value::Integer(left), Value::Integer(right), "<=") => Some(bool_value(left <= right)),
            (Value::Integer(left), Value::Integer(right), ">") => Some(bool_value(left > right)),
            (Value::Integer(left), Value::Integer(right), ">=") => Some(bool_value(left >= right)),
            (Value::Char(left), Value::Char(right), "==") => Some(bool_value(left == right)),
            (Value::Char(left), Value::Char(right), "!=") => Some(bool_value(left != right)),
            (Value::String(left), Value::String(right), "==") => Some(bool_value(left == right)),
            (Value::String(left), Value::String(right), "!=") => Some(bool_value(left != right)),
            (Value::Float(left, left_type), Value::Float(right, right_type), _) => self
                .fold_float_operator(
                    operator,
                    (*left, *left_type),
                    (*right, *right_type),
                    access.syntax.span.clone(),
                ),
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
        template: &staple_syntax::QuoteTemplate,
        environment: &Environment,
        mark: u64,
    ) -> Option<SyntaxValue> {
        match template {
            staple_syntax::QuoteTemplate::Expression(template) => {
                let mut expression = template.as_ref().clone();
                alpha_rename_expression(&mut expression, mark, &mut Vec::new());
                self.freshen_expression(&mut expression, module, mark);
                substitute_splices(&expression, environment, &mut self.diagnostics)
                    .map(SyntaxValue::from_expression)
            }
            staple_syntax::QuoteTemplate::Item(template) => {
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
            staple_syntax::QuoteTemplate::Items(templates) => {
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
            staple_syntax::QuoteTemplate::Raw => {
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
        quote: &staple_syntax::QuoteExpression,
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
                    (MacroKind::Quote, staple_syntax::QuoteKind::Quote)
                        | (MacroKind::ParseQuote, staple_syntax::QuoteKind::ParseQuote)
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
        if matches!(expected, MetaType::TypeParameter | MetaType::TypeParameters) {
            let syntax = self.substitute_raw_quote(module, &quote.contents, environment)?;
            return self.instantiate_substituted_fragment(module, &syntax, expected);
        }
        if *expected == MetaType::Type {
            return self.instantiate_type_quote(module, quote, environment);
        }
        if *expected == MetaType::Pattern {
            let mark = self.next_mark;
            self.next_mark += 1;
            let mut pattern = staple_syntax::parse_pattern_template_fragment(
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
                .any(|token| token.kind == staple_syntax::TokenKind::Dollar)
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
            .any(|token| token.kind == staple_syntax::TokenKind::Dollar)
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
            MetaType::TypeParameter | MetaType::TypeParameters | MetaType::Delimited(_, _) => {
                return match_syntax_fragment(expected, syntax, &mut self.next_syntax_id);
            }
            MetaType::SyntaxNode => structural_syntax_value(syntax, &mut self.next_syntax_id)?,
            MetaType::Expr => {
                let mut expression =
                    staple_syntax::parse_expression_fragment(syntax, &mut self.next_syntax_id)
                        .ok()?;
                alpha_rename_expression(&mut expression, mark, &mut Vec::new());
                self.freshen_expression(&mut expression, module, mark);
                SyntaxValue::from_expression(expression)
            }
            MetaType::Type => {
                let mut ty =
                    staple_syntax::parse_type_fragment(syntax, false, &mut self.next_syntax_id)
                        .ok()?;
                freshen_type(self, &mut ty, module, mark);
                SyntaxValue::Type(ty)
            }
            MetaType::Pattern => {
                let mut pattern =
                    staple_syntax::parse_pattern_fragment(syntax, false, &mut self.next_syntax_id)
                        .ok()?;
                freshen_pattern(self, &mut pattern, module, mark);
                SyntaxValue::Pattern(pattern)
            }
            MetaType::Item => {
                let mut item =
                    staple_syntax::parse_item_fragment(syntax, &mut self.next_syntax_id).ok()?;
                alpha_rename_item(&mut item, mark);
                freshen_item(self, &mut item, module, mark);
                SyntaxValue::Item(Box::new(item))
            }
            MetaType::Sequence(element) if **element == MetaType::Item => {
                let mut items =
                    staple_syntax::parse_item_list_fragment(syntax, &mut self.next_syntax_id)
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
        // The lossless token stream backing `contents` reconstructs the whole
        // source file `contents` was parsed from, so a token's `span.start` is a
        // real byte offset into it. Keep it to give the quotation-template tokens
        // an `origin` pointing back at the macro definition, the same way spliced
        // tokens already carry the caller's `origin`.
        let file_source: String = contents
            .token_stream()
            .iter()
            .map(|token| token.text.as_str())
            .collect();
        let template_origin = |token: &staple_syntax::SyntaxToken| match &contents.span {
            Span::User { source, .. } if !token.kind.is_trivia() => Some(Span::User {
                source: source.clone(),
                range: token.span.clone(),
                location: Some(source_location_at(&file_source, token.span.start)),
            }),
            _ => None,
        };
        let push_template = |output: &mut Vec<staple_syntax::SyntaxToken>, index: usize| {
            let mut token = input[index].clone();
            if token.origin.is_none() {
                token.origin = template_origin(&token);
            }
            output.push(token);
        };
        let mut output = Vec::new();
        let mut cursor = 0;
        while cursor < input.len() {
            if input[cursor].kind != staple_syntax::TokenKind::Dollar {
                push_template(&mut output, cursor);
                cursor += 1;
                continue;
            }
            let mut name_at = cursor + 1;
            while name_at < input.len() && input[name_at].kind.is_trivia() {
                name_at += 1;
            }
            if name_at == input.len() || input[name_at].kind != staple_syntax::TokenKind::Identifier
            {
                push_template(&mut output, cursor);
                cursor += 1;
                continue;
            }
            let mut end = name_at + 1;
            let repeated =
                end < input.len() && input[end].kind == staple_syntax::TokenKind::Ellipsis;
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
                        append_syntax_tokens(&mut output, item_syntax(&item));
                    }
                }
                Value::Sequence(values) if repeated => {
                    for value in values {
                        let Value::Syntax(value) = value else {
                            self.diagnostics.push(Diagnostic::new(
                                contents.span.clone(),
                                format!("repeated splice `${name}...` requires `Sequence<Item>`"),
                            ));
                            return None;
                        };
                        let syntax = value.syntax()?;
                        append_syntax_tokens(&mut output, syntax);
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
                    append_syntax_tokens(&mut output, syntax);
                }
                _ => {
                    self.diagnostics.push(Diagnostic::new(
                        contents.span.clone(),
                        if repeated {
                            format!("repeated splice `${name}...` requires `Sequence<Item>`")
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
        quote: &staple_syntax::QuoteExpression,
        environment: &Environment,
    ) -> Option<Value> {
        let mark = self.next_mark;
        self.next_mark += 1;
        let mut ty =
            staple_syntax::parse_type_template_fragment(&quote.contents, &mut self.next_syntax_id)
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
        let annotation_namespace = syntax.id.0 >> (usize::BITS - 2);
        syntax.id = if annotation_namespace != 0 && syntax.id != SyntaxId::COMPILER {
            // Generated annotation wrappers have a third namespace. A separate
            // cursor also handles nested wrappers after an empty parameter splice.
            let id = SyntaxId(usize::MAX - 1 - self.next_annotation_id);
            self.next_annotation_id += 1;
            id
        } else {
            self.fresh_id()
        };
        *syntax = syntax.clone().generated(module.0, mark);
    }

    fn freshen_expression(&mut self, expression: &mut Expression, module: ModuleId, mark: u64) {
        self.freshen_syntax(expression_syntax_mut(expression), module, mark);
        // A generated reference that lies outside the outermost invocation
        // reports that invocation as its caller location, like `std.source`.
        if matches!(
            expression,
            Expression::Name(_) | Expression::Access(_) | Expression::TypeApplication(_)
        ) && let Some(root) = &self.root_invocation_span
        {
            let syntax = expression_syntax_mut(expression);
            if !span_contains(root, &syntax.span) {
                self.caller_location_spans.insert(syntax.id, root.clone());
            }
        }
        match expression {
            Expression::Function(function) => {
                if let Some(generics) = &mut function.generics {
                    freshen_generics(self, generics, module, mark);
                }
                freshen_pattern(self, &mut function.pattern, module, mark);
                if let Some(effects) = &mut function.effect_lower_bound {
                    self.freshen_syntax(&mut effects.syntax, module, mark);
                    for resource in &mut effects.resources {
                        self.freshen_syntax(&mut resource.syntax, module, mark);
                        if let Some(binding) = &mut resource.binding {
                            self.freshen_syntax(&mut binding.syntax, module, mark);
                        }
                        freshen_type(self, &mut resource.value_type, module, mark);
                    }
                }
                self.freshen_expression(&mut function.body, module, mark);
            }
            Expression::TypeApplication(application) => {
                self.freshen_expression(&mut application.value, module, mark);
                for argument in &mut application.arguments {
                    freshen_type(self, argument, module, mark);
                }
            }
            Expression::TypeAscription(ascription) => {
                self.freshen_expression(&mut ascription.value, module, mark);
                freshen_type(self, &mut ascription.ty, module, mark);
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
            Expression::Coro(coro) => {
                self.freshen_syntax(&mut coro.body.syntax, module, mark);
                for item in &mut coro.body.items {
                    freshen_item(self, item, module, mark);
                }
            }
            Expression::Await(await_) => {
                self.freshen_expression(&mut await_.operand, module, mark);
            }
            Expression::Try(try_) => {
                self.freshen_syntax(&mut try_.operator_syntax, module, mark);
                self.freshen_expression(&mut try_.operand, module, mark);
            }
            Expression::Conversion(conversion) => {
                self.freshen_syntax(&mut conversion.operator_syntax, module, mark);
                self.freshen_expression(&mut conversion.value, module, mark);
                freshen_type(self, &mut conversion.ty, module, mark);
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
            Expression::RepeatedProduct(repeated) => {
                self.freshen_expression(&mut repeated.value, module, mark);
                freshen_type(self, &mut repeated.count, module, mark);
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
            Expression::Unary(unary) => {
                self.freshen_syntax(&mut unary.operator_syntax, module, mark);
                self.freshen_expression(&mut unary.operand, module, mark);
            }
            Expression::Binary(binary) => {
                self.freshen_syntax(&mut binary.operator_syntax, module, mark);
                self.freshen_expression(&mut binary.left, module, mark);
                self.freshen_expression(&mut binary.right, module, mark);
            }
            Expression::Pipe(pipe) => {
                self.freshen_syntax(&mut pipe.operator_syntax, module, mark);
                self.freshen_expression(&mut pipe.receiver, module, mark);
                self.freshen_expression(&mut pipe.stage, module, mark);
            }
            Expression::Logical(logical) => {
                self.freshen_expression(&mut logical.left, module, mark);
                self.freshen_expression(&mut logical.right, module, mark);
                freshen_type(self, &mut logical.bool_type, module, mark);
            }
            Expression::StringTemplate(template) => {
                for part in &mut template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
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
            | Expression::Char(_)
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
        MetaType::Sequence(_) | MetaType::Optional(_) | MetaType::SlottedProduct(_)
    ) {
        diagnostics.push(Diagnostic::new(
            definition.declaration.syntax.span.clone(),
            format!(
                "a top-level `Sequence` element must be a single syntax category, found `{}`",
                format_meta_type(element)
            ),
        ));
    }
    // A top-level `Sequence` may be the final parameter: with no required suffix
    // it greedily consumes the maximal run of consecutive matching arguments,
    // and a longer complete overload still wins the atom-count comparison.
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

fn macro_same_package(program: &Program, left: ModuleId, right: ModuleId) -> bool {
    program
        .package_of(left)
        .zip(program.package_of(right))
        .is_some_and(|(left, right)| left == right)
}

/// A copy of `declaration` whose value is curried into the nested
/// single-parameter functions expansion consumes: the source value
/// `(a; b) => body` becomes `a => b => body`. Inner layers get fresh syntax
/// identities.
fn with_curried_macro_value(
    declaration: &staple_syntax::MacroDeclaration,
    next_syntax_id: &mut usize,
) -> staple_syntax::MacroDeclaration {
    let mut declaration = declaration.clone();
    let Some(Expression::Function(function)) = &mut declaration.value else {
        return declaration;
    };
    let Pattern::Product(product) = &mut function.pattern else {
        return declaration;
    };
    if !product.slotted || product.elements.is_empty() {
        return declaration;
    }
    let mut parameters = std::mem::take(&mut product.elements);
    let first = parameters.remove(0);
    let mut body = std::mem::replace(
        &mut function.body,
        Box::new(Expression::Product(
            staple_syntax::ProductExpression::empty(),
        )),
    );
    for pattern in parameters.into_iter().rev() {
        body = Box::new(Expression::Function(Box::new(
            staple_syntax::FunctionExpression {
                syntax: freshened_syntax(&function.syntax, next_syntax_id),
                generics: None,
                pattern,
                effect_lower_bound: None,
                body,
            },
        )));
    }
    function.pattern = first;
    function.body = body;
    declaration
}

fn expression_arity(expression: &Expression) -> usize {
    match expression {
        Expression::Function(function) => 1 + expression_arity(&function.body),
        _ => 0,
    }
}

fn macro_annotation_arity(annotation: &Type) -> usize {
    match annotation {
        Type::Function(function) => {
            let this = match function.parameter.as_ref() {
                Type::Product(product) if product.slotted => product.elements.len(),
                _ => 1,
            };
            this + macro_annotation_arity(&function.result)
        }
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
            "CharExpr" => Some(MetaType::CharExpr),
            "StringExpr" => Some(MetaType::StringExpr),
            "UnstructuredExpr" => Some(MetaType::UnstructuredExpr),
            "Type" => Some(MetaType::Type),
            "Pattern" => Some(MetaType::Pattern),
            "BindingPattern" => Some(MetaType::BindingPattern),
            "NominalPattern" => Some(MetaType::NominalPattern),
            "Item" => Some(MetaType::Item),
            "Modifier" => Some(MetaType::Modifier),
            "ModifierCall" => Some(MetaType::ModifierCall),
            "UnstructuredModifier" => Some(MetaType::UnstructuredModifier),
            "ModifiedItem" => Some(MetaType::ModifiedItem),
            "TypeDeclarationItem" => Some(MetaType::TypeDeclarationItem),
            "TypeBody" => Some(MetaType::TypeBody),
            "TypeParameter" => Some(MetaType::TypeParameter),
            "TypeParameters" => Some(MetaType::TypeParameters),
            "UnstructuredItem" => Some(MetaType::UnstructuredItem),
            "Visibility" => Some(MetaType::Visibility),
            "MacroCallMetadata" => Some(MetaType::MacroCallMetadata),
            "Comma" => Some(MetaType::Comma),
            "Equals" => Some(MetaType::Equals),
            "FatArrow" => Some(MetaType::FatArrow),
            _ => None,
        },
        Type::Application(application) => {
            if let Some(element) = sequence_meta_type(ty) {
                return Some(MetaType::Sequence(Box::new(element)));
            }
            if let Some(element) = applied_meta_type(application, "Option") {
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
                Type::StringLiteral(literal) => {
                    staple_syntax::string_literal::decode(&literal.literal)
                        .ok()
                        .map(|spelling| MetaType::Ident(Some(spelling)))
                }
                _ => None,
            }
        }
        Type::Product(product)
            if product
                .elements
                .iter()
                .all(|element| element.name.is_none() && !element.spread) =>
        {
            let elements = product
                .elements
                .iter()
                .map(|element| meta_type(&element.ty))
                .collect::<Option<Vec<_>>>()?;
            // A slotted product juxtaposes its elements; a product is a
            // parenthesized, comma-separated group.
            Some(if product.slotted {
                MetaType::SlottedProduct(elements)
            } else {
                MetaType::Product(elements)
            })
        }
        _ => None,
    }
}

fn applied_meta_type(
    application: &staple_syntax::TypeApplication,
    expected: &str,
) -> Option<MetaType> {
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
        "Angled" => Some(DelimiterKind::Angled),
        "Braced" => Some(DelimiterKind::Braced),
        _ => None,
    }
}

fn delimiter_contents_meta(ty: &Type) -> Option<DelimitedMetaContents> {
    // A single element doesn't need the grouping parens that separate
    // multiple fixed contents, so `Parenthesized<Expr>` is read the same as
    // `Parenthesized<(Expr)>`.
    let Type::Product(product) = ty else {
        if let Some(element) = sequence_meta_type(ty) {
            return Some(DelimitedMetaContents::Sequence(Box::new(element)));
        }
        if let Some((element, separator)) = separated_meta_type(ty) {
            return Some(DelimitedMetaContents::Separated {
                element: Box::new(element),
                separator: Box::new(separator),
            });
        }
        return meta_type(ty).map(|single| DelimitedMetaContents::Slotted(vec![single]));
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
    let elements = product
        .elements
        .iter()
        .map(|element| {
            (!element.spread && element.name.is_none())
                .then(|| meta_type(&element.ty))
                .flatten()
        })
        .collect::<Option<Vec<_>>>()?;
    // Slotted contents `(A; B)` are juxtaposed, while product contents
    // `(A, B)` are separated by commas.
    Some(if product.slotted || elements.len() < 2 {
        DelimitedMetaContents::Slotted(elements)
    } else {
        DelimitedMetaContents::Product(elements)
    })
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

/// Reads `Sequence<T>` as a repeated syntax element of type `T`.
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
        _ => None,
    }
}

fn meta_type_contains_macro_call_metadata(meta: &MetaType) -> bool {
    match meta {
        MetaType::MacroCallMetadata => true,
        MetaType::SlottedProduct(elements) | MetaType::Product(elements) => {
            elements.iter().any(meta_type_contains_macro_call_metadata)
        }
        MetaType::Optional(element) | MetaType::Sequence(element) => {
            meta_type_contains_macro_call_metadata(element)
        }
        MetaType::Delimited(_, contents) => match contents {
            DelimitedMetaContents::Slotted(elements) | DelimitedMetaContents::Product(elements) => {
                elements.iter().any(meta_type_contains_macro_call_metadata)
            }
            DelimitedMetaContents::Sequence(element) => {
                meta_type_contains_macro_call_metadata(element)
            }
            DelimitedMetaContents::Separated { element, separator } => {
                meta_type_contains_macro_call_metadata(element)
                    || meta_type_contains_macro_call_metadata(separator)
            }
        },
        _ => false,
    }
}

fn meta_type_matches(expected: &MetaType, argument: &Expression) -> bool {
    let expression_argument = meta_argument_expression(expected, argument);
    match expected {
        MetaType::TypeParameter | MetaType::TypeParameters => {
            let mut next_syntax_id = 0;
            match_syntax_fragment(expected, argument.syntax(), &mut next_syntax_id).is_some()
        }
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
        MetaType::MacroCallMetadata | MetaType::ResourceDependency => false,
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
        MetaType::CharExpr => matches!(expression_argument, Expression::Char(_)),
        MetaType::StringExpr => matches!(expression_argument, Expression::String(_)),
        MetaType::UnstructuredExpr => !matches!(
            expression_argument,
            Expression::Name(_) | Expression::Call(_) | Expression::VisibilityArgument(_)
        ),
        MetaType::Item
        | MetaType::Modifier
        | MetaType::ModifierCall
        | MetaType::UnstructuredModifier
        | MetaType::ModifiedItem
        | MetaType::TypeDeclarationItem
        | MetaType::TypeBody
        | MetaType::UnstructuredItem
        | MetaType::SlottedProduct(_)
        | MetaType::Nominal(_, _)
        | MetaType::Optional(_)
        | MetaType::Sequence(_) => false,
        MetaType::Comma => matches_single_token(argument.syntax(), staple_syntax::TokenKind::Comma),
        MetaType::Equals => {
            matches_single_token(argument.syntax(), staple_syntax::TokenKind::Equals)
        }
        MetaType::FatArrow => {
            matches_single_token(argument.syntax(), staple_syntax::TokenKind::FatArrow)
        }
        MetaType::Delimited(_, _) => {
            let mut next_syntax_id = 0;
            delimiter_argument_value(expected, argument.syntax(), &mut next_syntax_id).is_some()
        }
        MetaType::Product(_) => {
            let mut next_syntax_id = 0;
            match_syntax_fragment(expected, argument.syntax(), &mut next_syntax_id).is_some()
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
    let kind = if *expected_kind == DelimiterKind::Angled {
        if tokens[first].text != "<" || matching_angle_close(tokens, first) != Some(last) {
            return None;
        }
        DelimiterKind::Angled
    } else {
        match (tokens[first].kind, tokens[last].kind) {
            (staple_syntax::TokenKind::LParen, staple_syntax::TokenKind::RParen) => {
                DelimiterKind::Parenthesized
            }
            (staple_syntax::TokenKind::LBracket, staple_syntax::TokenKind::RBracket) => {
                DelimiterKind::Bracketed
            }
            (staple_syntax::TokenKind::LBrace, staple_syntax::TokenKind::RBrace) => {
                DelimiterKind::Braced
            }
            _ => return None,
        }
    };
    if kind != *expected_kind {
        return None;
    }
    let contents = match expected_contents {
        DelimitedMetaContents::Slotted(elements) if elements.as_slice() == [MetaType::Syntax] => {
            Some(DelimitedValueContents::Slotted(vec![(
                None,
                Value::Syntax(SyntaxValue::Raw(syntax_slice(syntax, first + 1, last))),
            )]))
        }
        DelimitedMetaContents::Slotted(elements) => {
            match_slotted_contents(syntax, first + 1, last, elements, next_syntax_id)
                .map(DelimitedValueContents::Slotted)
        }
        DelimitedMetaContents::Product(elements) => {
            match_product_contents(syntax, first + 1, last, elements, next_syntax_id)
                .map(DelimitedValueContents::Product)
        }
        DelimitedMetaContents::Sequence(element) => {
            match_sequence_contents(syntax, first + 1, last, element, next_syntax_id)
                .map(|values| DelimitedValueContents::Sequence(values.into()))
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
    let expression = staple_syntax::parse_expression_fragment(syntax, next_syntax_id)
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
    let modifier_prefixed = matches!(element, MetaType::SlottedProduct(elements) if matches!(elements.first(), Some(MetaType::Sequence(element)) if **element == MetaType::Modifier));
    if !modifier_prefixed {
        cursor = skip_trivia(parent.tokens(), cursor, end);
    }
    let separator_value = constructed_separator(separator)?;
    if skip_trivia(parent.tokens(), cursor, end) == end {
        return Some((Vec::new(), separator_value, false));
    }
    let mut elements = Vec::new();
    loop {
        let (value, separator_at) =
            match_before_separator(parent, cursor, end, element, separator, next_syntax_id)?;
        elements.push(value);
        let Some(separator_at) = separator_at else {
            return Some((elements, separator_value, false));
        };
        let separator_end = separator_at + 1;
        let separator_fragment = syntax_slice(parent, separator_at, separator_end);
        match_syntax_fragment(separator, &separator_fragment, next_syntax_id)?;
        cursor = if modifier_prefixed {
            separator_end
        } else {
            skip_trivia(parent.tokens(), separator_end, end)
        };
        if skip_trivia(parent.tokens(), cursor, end) == end {
            return Some((elements, separator_value, true));
        }
    }
}

/// Let the expected syntax grammar decide which comma ends an element. A comma
/// inside `Map<K, V>` is not a separator, while commas after comparisons are.
fn match_before_separator(
    parent: &Syntax,
    cursor: usize,
    end: usize,
    element: &MetaType,
    separator: &MetaType,
    next_syntax_id: &mut usize,
) -> Option<(Value, Option<usize>)> {
    let mut search = cursor;
    loop {
        let separator_at = find_top_level_separator(parent.tokens(), search, end, separator);
        let element_end =
            trim_trailing_trivia(parent.tokens(), cursor, separator_at.unwrap_or(end));
        let checkpoint = *next_syntax_id;
        if element_end > cursor {
            let fragment = syntax_slice(parent, cursor, element_end);
            if let Some(value) = match_syntax_fragment(element, &fragment, next_syntax_id) {
                return Some((value, separator_at));
            }
        }
        *next_syntax_id = checkpoint;
        search = separator_at? + 1;
    }
}

fn constructed_separator(separator: &MetaType) -> Option<Value> {
    match separator {
        MetaType::Comma => Some(Value::Syntax(SyntaxValue::Comma(Syntax::compiler()))),
        _ => None,
    }
}

fn find_top_level_separator(
    tokens: &[staple_syntax::SyntaxToken],
    cursor: usize,
    end: usize,
    separator: &MetaType,
) -> Option<usize> {
    let separator_kind = match separator {
        MetaType::Comma => staple_syntax::TokenKind::Comma,
        _ => return None,
    };
    let mut delimiters = Vec::new();
    for (index, token) in tokens.iter().enumerate().take(end).skip(cursor) {
        match token.kind {
            staple_syntax::TokenKind::LParen => delimiters.push(staple_syntax::TokenKind::RParen),
            staple_syntax::TokenKind::LBracket => {
                delimiters.push(staple_syntax::TokenKind::RBracket)
            }
            staple_syntax::TokenKind::LBrace => delimiters.push(staple_syntax::TokenKind::RBrace),
            staple_syntax::TokenKind::RParen
            | staple_syntax::TokenKind::RBracket
            | staple_syntax::TokenKind::RBrace => {
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

fn trim_trailing_trivia(
    tokens: &[staple_syntax::SyntaxToken],
    start: usize,
    mut end: usize,
) -> usize {
    while end > start && tokens[end - 1].kind.is_trivia() {
        end -= 1;
    }
    end
}

/// Matches comma-separated contents against a product shape: exactly one
/// element per expected shape, with an optional trailing comma.
fn match_product_contents(
    parent: &Syntax,
    mut cursor: usize,
    end: usize,
    expected: &[MetaType],
    next_syntax_id: &mut usize,
) -> Option<Vec<(Option<String>, Value)>> {
    let checkpoint = *next_syntax_id;
    let mut values = Vec::with_capacity(expected.len());
    for (index, element) in expected.iter().enumerate() {
        cursor = skip_trivia(parent.tokens(), cursor, end);
        let Some((value, separator_at)) = match_before_separator(
            parent,
            cursor,
            end,
            element,
            &MetaType::Comma,
            next_syntax_id,
        ) else {
            *next_syntax_id = checkpoint;
            return None;
        };
        values.push((None, value));
        match separator_at {
            Some(separator_at) => cursor = separator_at + 1,
            None if index + 1 == expected.len() => cursor = end,
            None => {
                *next_syntax_id = checkpoint;
                return None;
            }
        }
    }
    if skip_trivia(parent.tokens(), cursor, end) != end {
        *next_syntax_id = checkpoint;
        return None;
    }
    Some(values)
}

fn match_slotted_contents(
    parent: &Syntax,
    cursor: usize,
    end: usize,
    expected: &[MetaType],
    next_syntax_id: &mut usize,
) -> Option<Vec<(Option<String>, Value)>> {
    let Some((first, rest)) = expected.split_first() else {
        return (skip_trivia(parent.tokens(), cursor, end) == end).then(Vec::new);
    };
    let cursor = if matches!(first, MetaType::Sequence(element) if **element == MetaType::Modifier)
    {
        cursor
    } else {
        skip_trivia(parent.tokens(), cursor, end)
    };
    if let MetaType::Sequence(element) = first {
        if **element == MetaType::Modifier
            && let Some((docs, doc_end)) = leading_doc_modifiers(parent, cursor, end)
        {
            let checkpoint = *next_syntax_id;
            if let Some(mut values) =
                match_slotted_contents(parent, doc_end, end, expected, next_syntax_id)
                && let Some((_, Value::Sequence(sequence))) = values.first_mut()
            {
                for doc in docs.into_iter().rev() {
                    sequence.insert(
                        0,
                        Value::Syntax(SyntaxValue::Modifier(OpaqueModifier {
                            invocation: doc,
                            enclosing: None,
                        })),
                    );
                }
                return Some(values);
            }
            *next_syntax_id = checkpoint;
            return None;
        }
        let cursor = skip_trivia(parent.tokens(), cursor, end);
        let checkpoint = *next_syntax_id;
        if let Some(mut values) = match_slotted_contents(parent, cursor, end, rest, next_syntax_id)
        {
            values.insert(0, (None, Value::Sequence(SequenceValue::default())));
            return Some(values);
        }
        *next_syntax_id = checkpoint;
        for candidate_end in candidate_ends(parent.tokens(), cursor, end) {
            let fragment = syntax_slice(parent, cursor, candidate_end);
            let checkpoint = *next_syntax_id;
            if let Some(value) = match_syntax_fragment(element, &fragment, next_syntax_id)
                && let Some(mut values) =
                    match_slotted_contents(parent, candidate_end, end, expected, next_syntax_id)
                && let Some((_, Value::Sequence(sequence))) = values.first_mut()
            {
                sequence.insert(0, value);
                return Some(values);
            }
            *next_syntax_id = checkpoint;
        }
        return None;
    }
    if matches!(first, MetaType::Optional(_)) {
        let checkpoint = *next_syntax_id;
        if let Some(mut values) = match_slotted_contents(parent, cursor, end, rest, next_syntax_id)
        {
            values.insert(
                0,
                (
                    None,
                    Value::Nominal(
                        "None".to_owned(),
                        Box::new(Value::Product(Vec::new().into(), false)),
                    ),
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
                match_slotted_contents(parent, candidate_end, end, rest, next_syntax_id)
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
        if *expected == MetaType::Modifier
            && let Some((docs, doc_end)) = leading_doc_modifiers(parent, cursor, end)
        {
            for doc in docs {
                values.push(Value::Syntax(SyntaxValue::Modifier(OpaqueModifier {
                    invocation: doc,
                    enclosing: None,
                })));
            }
            cursor = doc_end;
            continue;
        }
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

fn leading_doc_modifiers(
    parent: &Syntax,
    start: usize,
    end: usize,
) -> Option<(Vec<staple_syntax::ModifierInvocation>, usize)> {
    let tokens = parent.tokens();
    let mut position = start;
    let mut line_start = start == 0
        || tokens[..start]
            .last()
            .is_some_and(|token| token.kind == staple_syntax::TokenKind::Newline);
    let mut docs = Vec::new();
    while position < end {
        let token = &tokens[position];
        match token.kind {
            staple_syntax::TokenKind::Whitespace => position += 1,
            staple_syntax::TokenKind::Newline => {
                line_start = true;
                position += 1;
            }
            staple_syntax::TokenKind::LineComment
                if line_start
                    && token.text.starts_with("///")
                    && !token.text.starts_with("////") =>
            {
                docs.push(staple_syntax::ModifierInvocation {
                    syntax: syntax_slice(parent, start, position + 1),
                    namespace: None,
                    name: "doc".to_owned(),
                    argument: None,
                    doc: Some(token.text[3..].to_owned()),
                });
                line_start = false;
                position += 1;
            }
            _ => break,
        }
    }
    (!docs.is_empty()).then_some((docs, position))
}

fn match_syntax_fragment(
    expected: &MetaType,
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Option<Value> {
    match expected {
        MetaType::SlottedProduct(elements) => {
            let values =
                match_slotted_contents(syntax, 0, syntax.tokens().len(), elements, next_syntax_id)?;
            return Some(Value::Product(values.into(), true));
        }
        MetaType::Product(elements) => {
            let tokens = syntax.tokens();
            let first = tokens.iter().position(|token| !token.kind.is_trivia())?;
            let last = tokens.iter().rposition(|token| !token.kind.is_trivia())?;
            if tokens[first].kind != staple_syntax::TokenKind::LParen
                || tokens[last].kind != staple_syntax::TokenKind::RParen
                || matching_close(tokens, first) != Some(last)
            {
                return None;
            }
            let values = match_product_contents(syntax, first + 1, last, elements, next_syntax_id)?;
            return Some(Value::Product(values.into(), false));
        }
        MetaType::Optional(element) => {
            let empty = syntax.tokens().iter().all(|token| token.kind.is_trivia());
            return if empty {
                Some(Value::Nominal(
                    "None".to_owned(),
                    Box::new(Value::Product(Vec::new().into(), false)),
                ))
            } else {
                match_syntax_fragment(element, syntax, next_syntax_id)
                    .map(|value| Value::Nominal("Some".to_owned(), Box::new(value)))
            };
        }
        MetaType::Sequence(element) => {
            let values =
                match_sequence_contents(syntax, 0, syntax.tokens().len(), element, next_syntax_id)?;
            return Some(Value::Sequence(values.into()));
        }
        MetaType::TypeParameter | MetaType::TypeParameters => {
            let (value, consumed) =
                parse_parameter_syntax(expected, syntax, next_syntax_id).ok()?;
            return (skip_trivia(syntax.tokens(), consumed, syntax.tokens().len())
                == syntax.tokens().len())
            .then_some(value);
        }
        MetaType::TypeBody => {
            let body = staple_syntax::parse_type_body_fragment(syntax, next_syntax_id).ok()?;
            return Some(type_body_value(&body));
        }
        _ => {}
    }
    let expression = || {
        let mut ids = *next_syntax_id;
        let value = staple_syntax::parse_expression_fragment(syntax, &mut ids).ok()?;
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
            if token.kind != staple_syntax::TokenKind::Identifier
                || tokens.next().is_some()
                || spelling
                    .as_ref()
                    .is_some_and(|expected| expected != &token.text)
            {
                return None;
            }
            SyntaxValue::Ident(staple_syntax::NameExpression {
                syntax: syntax.clone(),
                name: token.text.clone(),
            })
        }
        MetaType::Comma => {
            if !matches_single_token(syntax, staple_syntax::TokenKind::Comma) {
                return None;
            }
            SyntaxValue::Comma(syntax.clone())
        }
        MetaType::Equals => {
            if !matches_single_token(syntax, staple_syntax::TokenKind::Equals) {
                return None;
            }
            SyntaxValue::Equals(syntax.clone())
        }
        MetaType::FatArrow => {
            if !matches_single_token(syntax, staple_syntax::TokenKind::FatArrow) {
                return None;
            }
            SyntaxValue::FatArrow(syntax.clone())
        }
        MetaType::Expr
        | MetaType::CallExpr
        | MetaType::CharExpr
        | MetaType::StringExpr
        | MetaType::UnstructuredExpr => {
            let (value, ids) = expression()?;
            let value = SyntaxValue::from_expression(value);
            if matches!(expected, MetaType::CallExpr) && !matches!(value, SyntaxValue::Call(_))
                || matches!(expected, MetaType::CharExpr)
                    && !matches!(value, SyntaxValue::Unstructured(Expression::Char(_)))
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
            staple_syntax::parse_type_fragment(syntax, false, next_syntax_id).ok()?,
        ),
        MetaType::Pattern | MetaType::BindingPattern | MetaType::NominalPattern => {
            let pattern =
                staple_syntax::parse_pattern_fragment(syntax, false, next_syntax_id).ok()?;
            if matches!(expected, MetaType::BindingPattern)
                && !matches!(pattern, Pattern::Binding(_))
                || matches!(expected, MetaType::NominalPattern)
                    && !matches!(pattern, Pattern::Nominal(_))
            {
                return None;
            }
            SyntaxValue::Pattern(pattern)
        }
        MetaType::Item
        | MetaType::ModifiedItem
        | MetaType::TypeDeclarationItem
        | MetaType::UnstructuredItem => {
            let item = staple_syntax::parse_item_fragment(syntax, next_syntax_id).ok()?;
            if matches!(expected, MetaType::ModifiedItem) && !matches!(item, Item::Modified(_))
                || matches!(expected, MetaType::TypeDeclarationItem)
                    && !matches!(item, Item::TypeDeclaration(_))
                || matches!(expected, MetaType::UnstructuredItem)
                    && matches!(item, Item::Modified(_) | Item::TypeDeclaration(_))
            {
                return None;
            }
            SyntaxValue::Item(Box::new(item))
        }
        MetaType::Modifier | MetaType::ModifierCall | MetaType::UnstructuredModifier => {
            let invocation = staple_syntax::parse_modifier_fragment(syntax, next_syntax_id).ok()?;
            if matches!(expected, MetaType::ModifierCall) && invocation.doc.is_some()
                || matches!(expected, MetaType::UnstructuredModifier) && invocation.doc.is_none()
            {
                return None;
            }
            SyntaxValue::Modifier(OpaqueModifier {
                invocation,
                enclosing: None,
            })
        }
        MetaType::Visibility => {
            let (value, ids) = expression()?;
            let Expression::VisibilityArgument(value) = value else {
                return None;
            };
            *next_syntax_id = ids;
            SyntaxValue::Visibility(value)
        }
        MetaType::Syntax => SyntaxValue::Raw(syntax.clone()),
        MetaType::SyntaxNode => structural_syntax_value(syntax, next_syntax_id)?,
        MetaType::SlottedProduct(_)
        | MetaType::Product(_)
        | MetaType::Optional(_)
        | MetaType::Nominal(_, _)
        | MetaType::Sequence(_) => unreachable!(),
        MetaType::TypeBody | MetaType::TypeParameter | MetaType::TypeParameters => {
            unreachable!("declaration syntax is matched before expression parsing")
        }
        MetaType::MacroCallMetadata => unreachable!(),
        // Only built from a declaration's clause; it has no syntax to parse.
        MetaType::ResourceDependency => return None,
    };
    Some(Value::Syntax(syntax_value))
}

fn matches_single_token(syntax: &Syntax, expected: staple_syntax::TokenKind) -> bool {
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
        && token.kind == staple_syntax::TokenKind::Identifier
    {
        return Some(SyntaxValue::Ident(staple_syntax::NameExpression {
            syntax: syntax.clone(),
            name: token.text.clone(),
        }));
    }
    if let [token] = tokens.as_slice()
        && token.kind == staple_syntax::TokenKind::Comma
    {
        return Some(SyntaxValue::Comma(syntax.clone()));
    }
    if let [token] = tokens.as_slice()
        && token.kind == staple_syntax::TokenKind::Equals
    {
        return Some(SyntaxValue::Equals(syntax.clone()));
    }
    if let [token] = tokens.as_slice()
        && token.kind == staple_syntax::TokenKind::FatArrow
    {
        return Some(SyntaxValue::FatArrow(syntax.clone()));
    }
    if let Some((open, close, kind)) =
        tokens
            .first()
            .zip(tokens.last())
            .and_then(|(open, close)| match (open.kind, close.kind) {
                (staple_syntax::TokenKind::LParen, staple_syntax::TokenKind::RParen) => {
                    Some((open, close, DelimiterKind::Parenthesized))
                }
                (staple_syntax::TokenKind::LBracket, staple_syntax::TokenKind::RBracket) => {
                    Some((open, close, DelimiterKind::Bracketed))
                }
                (staple_syntax::TokenKind::LBrace, staple_syntax::TokenKind::RBrace) => {
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
    if let Ok(expression) = staple_syntax::parse_expression_fragment(syntax, next_syntax_id) {
        return Some(SyntaxValue::from_expression(expression));
    }
    if let Ok(item) = staple_syntax::parse_item_fragment(syntax, next_syntax_id) {
        return Some(SyntaxValue::Item(Box::new(item)));
    }
    if let Ok(ty) = staple_syntax::parse_type_fragment(syntax, false, next_syntax_id) {
        return Some(SyntaxValue::Type(ty));
    }
    staple_syntax::parse_pattern_fragment(syntax, false, next_syntax_id)
        .ok()
        .map(SyntaxValue::Pattern)
}

fn skip_trivia(tokens: &[staple_syntax::SyntaxToken], mut cursor: usize, end: usize) -> usize {
    while cursor < end && tokens[cursor].kind.is_trivia() {
        cursor += 1;
    }
    cursor
}

/// Finds the token that closes the delimiter opened at `open`.
fn matching_angle_close(tokens: &[staple_syntax::SyntaxToken], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut cursor = open;
    while let Some(token) = tokens.get(cursor) {
        match token.kind {
            staple_syntax::TokenKind::LParen
            | staple_syntax::TokenKind::LBracket
            | staple_syntax::TokenKind::LBrace => {
                cursor = matching_close(tokens, cursor)?;
            }
            staple_syntax::TokenKind::Operator if token.text == "<" => depth += 1,
            staple_syntax::TokenKind::Operator if token.text == ">" => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn matching_close(tokens: &[staple_syntax::SyntaxToken], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            staple_syntax::TokenKind::LParen
            | staple_syntax::TokenKind::LBracket
            | staple_syntax::TokenKind::LBrace => depth += 1,
            staple_syntax::TokenKind::RParen
            | staple_syntax::TokenKind::RBracket
            | staple_syntax::TokenKind::RBrace => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn candidate_ends(
    tokens: &[staple_syntax::SyntaxToken],
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
    call_metadata: Option<&MacroCallMetadata>,
) -> Option<(Vec<MacroArgument>, usize, Vec<(MetaType, bool)>)> {
    let mut matched = Vec::with_capacity(definition.parameters.len());
    let mut effective = Vec::new();
    let mut parameter_index = 0;

    if definition.parameters.first() == Some(&MetaType::MacroCallMetadata) {
        matched.push(MacroArgument::Metadata(call_metadata.cloned().unwrap_or(
            MacroCallMetadata {
                syntax: Syntax::compiler(),
                modifiers: Vec::new(),
                visibility: private_visibility(),
            },
        )));
        parameter_index += 1;
    } else if call_metadata.is_some() {
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
        MetaType::Optional(element) => {
            let checkpoint = matched.len();
            let effective_checkpoint = effective.len();
            if let Some(argument) = arguments.get(argument_index) {
                let mut ids = 0;
                if let Some(value) = match_syntax_fragment(element, argument.syntax(), &mut ids) {
                    matched.push(MacroArgument::Parsed(Value::Nominal(
                        "Some".into(),
                        Box::new(value),
                    )));
                    effective.push(((**element).clone(), false));
                    if let Some(consumed) = match_macro_parameter_suffix(
                        rest,
                        arguments,
                        argument_index + 1,
                        matched,
                        effective,
                    ) {
                        return Some(consumed);
                    }
                    matched.truncate(checkpoint);
                    effective.truncate(effective_checkpoint);
                }
            }
            matched.push(MacroArgument::Parsed(Value::Nominal(
                "None".into(),
                Box::new(Value::Product(vec![].into(), false)),
            )));
            let result =
                match_macro_parameter_suffix(rest, arguments, argument_index, matched, effective);
            if result.is_none() {
                matched.truncate(checkpoint);
            }
            result
        }
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
        MetaType::MacroCallMetadata => None,
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
/// spelled `Parenthesized<(...)>` to match how every other delimited macro
/// argument is declared, down to the single meta type it wraps.
fn modifier_argument_meta_type(declared: &MetaType) -> Option<&MetaType> {
    if matches!(declared, MetaType::Delimited(DelimiterKind::Parenthesized, DelimitedMetaContents::Separated { element, separator }) if **element == MetaType::Modifier && **separator == MetaType::Comma)
    {
        return Some(declared);
    }
    let MetaType::Delimited(DelimiterKind::Parenthesized, DelimitedMetaContents::Slotted(inner)) =
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
    parse_modifier_argument_value(expected, argument, &mut 0).is_some()
}

fn valid_modifier_argument(expected: &MetaType) -> bool {
    matches!(
        expected,
        MetaType::Expr
            | MetaType::Ident(_)
            | MetaType::CallExpr
            | MetaType::UnstructuredExpr
            | MetaType::Type
            | MetaType::Pattern
            | MetaType::Modifier
    ) || matches!(expected, MetaType::Delimited(DelimiterKind::Parenthesized, DelimitedMetaContents::Separated { element, separator }) if **element == MetaType::Modifier && **separator == MetaType::Comma)
}

fn parse_modifier_argument_value(
    expected: &MetaType,
    argument: &ModifierArgument,
    ids: &mut usize,
) -> Option<Value> {
    match expected {
        MetaType::Type => staple_syntax::parse_type_fragment(&argument.syntax, true, ids)
            .ok()
            .map(|value| Value::Syntax(SyntaxValue::Type(value))),
        MetaType::Pattern => staple_syntax::parse_pattern_fragment(&argument.syntax, true, ids)
            .ok()
            .map(|value| Value::Syntax(SyntaxValue::Pattern(value))),
        MetaType::Modifier => {
            let tokens = argument.syntax.tokens();
            let first = tokens.iter().position(|token| !token.kind.is_trivia())?;
            let last = tokens.iter().rposition(|token| !token.kind.is_trivia())?;
            match_syntax_fragment(
                expected,
                &syntax_slice(&argument.syntax, first + 1, last),
                ids,
            )
        }
        MetaType::Delimited(_, DelimitedMetaContents::Separated { .. }) => {
            let SyntaxValue::Delimited(value) =
                delimiter_argument_value(expected, &argument.syntax, ids)?
            else {
                return None;
            };
            let DelimitedValueContents::Separated {
                elements,
                separator,
                trailing,
            } = value.contents
            else {
                return None;
            };
            Some(Value::Separated {
                elements,
                separator,
                trailing,
            })
        }
        _ if valid_modifier_argument(expected) => argument
            .expression
            .as_ref()
            .filter(|expression| meta_type_matches(expected, expression))
            .map(|expression| {
                Value::Syntax(SyntaxValue::from_expression(
                    meta_argument_expression(expected, expression).clone(),
                ))
            }),
        _ => None,
    }
}

fn category_argument_syntax(argument: &Expression) -> Option<(&Syntax, bool)> {
    match argument {
        Expression::SyntaxArgument(argument) => Some((&argument.syntax, true)),
        Expression::Product(product) => Some((&product.syntax, true)),
        Expression::Name(name) => Some((&name.syntax, false)),
        Expression::Access(access) => Some((&access.syntax, false)),
        Expression::Char(c) => Some((&c.syntax, false)),
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
    if grouped && let Ok(ty) = staple_syntax::parse_type_fragment(syntax, true, next_syntax_id) {
        return Some(ty);
    }
    staple_syntax::parse_type_fragment(syntax, false, next_syntax_id).ok()
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
        && let Ok(pattern) = staple_syntax::parse_pattern_fragment(syntax, true, next_syntax_id)
    {
        return Some(pattern);
    }
    staple_syntax::parse_pattern_fragment(syntax, false, next_syntax_id).ok()
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
            (MetaType::SlottedProduct(left), MetaType::SlottedProduct(right))
            | (MetaType::Product(left), MetaType::Product(right)) => {
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
        )
}

fn contents_at_least_as_specific(
    left: &DelimitedMetaContents,
    right: &DelimitedMetaContents,
) -> bool {
    match (left, right) {
        (DelimitedMetaContents::Slotted(left), DelimitedMetaContents::Slotted(right))
        | (DelimitedMetaContents::Product(left), DelimitedMetaContents::Product(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| meta_type_at_least_as_specific(left, right))
        }
        (DelimitedMetaContents::Sequence(left), DelimitedMetaContents::Sequence(right)) => {
            meta_type_at_least_as_specific(left, right)
        }
        (DelimitedMetaContents::Slotted(left), DelimitedMetaContents::Sequence(right)) => left
            .iter()
            .all(|left| meta_type_at_least_as_specific(left, right)),
        (DelimitedMetaContents::Sequence(_), DelimitedMetaContents::Slotted(_)) => false,
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
            DelimitedMetaContents::Slotted(left),
            DelimitedMetaContents::Separated { element, separator },
        ) => fixed_matches_separated(left, element, separator),
        (
            DelimitedMetaContents::Product(left),
            DelimitedMetaContents::Separated { element, .. },
        ) => left
            .iter()
            .all(|left| meta_type_at_least_as_specific(left, element)),
        (DelimitedMetaContents::Sequence(_), DelimitedMetaContents::Separated { .. })
        | (DelimitedMetaContents::Separated { .. }, DelimitedMetaContents::Slotted(_))
        | (DelimitedMetaContents::Separated { .. }, DelimitedMetaContents::Product(_))
        | (DelimitedMetaContents::Product(_), DelimitedMetaContents::Slotted(_))
        | (DelimitedMetaContents::Product(_), DelimitedMetaContents::Sequence(_))
        | (DelimitedMetaContents::Slotted(_), DelimitedMetaContents::Product(_))
        | (DelimitedMetaContents::Sequence(_), DelimitedMetaContents::Product(_)) => false,
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
    let rendered = parameters
        .iter()
        .map(|parameter| match parameter {
            MetaType::Syntax => "Syntax".to_owned(),
            MetaType::SyntaxNode => "SyntaxNode".to_owned(),
            MetaType::Expr => "Expr".to_owned(),
            MetaType::Ident(None) => "Ident<String>".to_owned(),
            MetaType::Ident(Some(spelling)) => format!("Ident<{spelling:?}>"),
            MetaType::CallExpr => "CallExpr".to_owned(),
            MetaType::CharExpr => "CharExpr".to_owned(),
            MetaType::StringExpr => "StringExpr".to_owned(),
            MetaType::UnstructuredExpr => "UnstructuredExpr".to_owned(),
            MetaType::Type => "Type".to_owned(),
            MetaType::Pattern => "Pattern".to_owned(),
            MetaType::BindingPattern => "BindingPattern".to_owned(),
            MetaType::NominalPattern => "NominalPattern".to_owned(),
            MetaType::Item => "Item".to_owned(),
            MetaType::Modifier => "Modifier".to_owned(),
            MetaType::ModifierCall => "ModifierCall".to_owned(),
            MetaType::UnstructuredModifier => "UnstructuredModifier".to_owned(),
            MetaType::ModifiedItem => "ModifiedItem".to_owned(),
            MetaType::TypeDeclarationItem => "TypeDeclarationItem".to_owned(),
            MetaType::TypeBody => "TypeBody".to_owned(),
            MetaType::TypeParameter => "TypeParameter".to_owned(),
            MetaType::TypeParameters => "TypeParameters".to_owned(),
            MetaType::ResourceDependency => "ResourceDependency".to_owned(),
            MetaType::UnstructuredItem => "UnstructuredItem".to_owned(),
            MetaType::Visibility => "Visibility".to_owned(),
            MetaType::MacroCallMetadata => "MacroCallMetadata".to_owned(),
            MetaType::Comma => "Comma".to_owned(),
            MetaType::Equals => "Equals".to_owned(),
            MetaType::FatArrow => "FatArrow".to_owned(),
            MetaType::SlottedProduct(_)
            | MetaType::Product(_)
            | MetaType::Optional(_)
            | MetaType::Nominal(_, _)
            | MetaType::Sequence(_) => format_meta_type(parameter),
            MetaType::Delimited(_, _) => format_meta_type(parameter),
        })
        .collect::<Vec<_>>()
        .join("; ");
    if parameters.len() > 1 {
        format!("({rendered})")
    } else {
        rendered
    }
}

fn resolved_macro(definition: &MacroDefinition) -> ResolvedMacro {
    if matches!(definition.kind, MacroKind::Quote | MacroKind::ParseQuote)
        && let Some(annotation) = &definition.declaration.annotation
    {
        let parameters = definition
            .declaration
            .type_parameters
            .iter()
            .flat_map(staple_syntax::TypeParameterPattern::names)
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
            modifier_vocabulary: Vec::new(),
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
        modifier_vocabulary: definition
            .declaration
            .modifier_vocabulary
            .clone()
            .unwrap_or_default(),
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
        MetaType::Ident(None) => "Ident<String>".to_owned(),
        MetaType::Ident(Some(spelling)) => format!("Ident<{spelling:?}>"),
        MetaType::CallExpr => "CallExpr".to_owned(),
        MetaType::CharExpr => "CharExpr".to_owned(),
        MetaType::StringExpr => "StringExpr".to_owned(),
        MetaType::UnstructuredExpr => "UnstructuredExpr".to_owned(),
        MetaType::Type => "Type".to_owned(),
        MetaType::Pattern => "Pattern".to_owned(),
        MetaType::BindingPattern => "BindingPattern".to_owned(),
        MetaType::NominalPattern => "NominalPattern".to_owned(),
        MetaType::Item => "Item".to_owned(),
        MetaType::Modifier => "Modifier".to_owned(),
        MetaType::ModifierCall => "ModifierCall".to_owned(),
        MetaType::UnstructuredModifier => "UnstructuredModifier".to_owned(),
        MetaType::ModifiedItem => "ModifiedItem".to_owned(),
        MetaType::TypeDeclarationItem => "TypeDeclarationItem".to_owned(),
        MetaType::TypeBody => "TypeBody".to_owned(),
        MetaType::TypeParameter => "TypeParameter".to_owned(),
        MetaType::TypeParameters => "TypeParameters".to_owned(),
        MetaType::ResourceDependency => "ResourceDependency".to_owned(),
        MetaType::UnstructuredItem => "UnstructuredItem".to_owned(),
        MetaType::Visibility => "Visibility".to_owned(),
        MetaType::MacroCallMetadata => "MacroCallMetadata".to_owned(),
        MetaType::Comma => "Comma".to_owned(),
        MetaType::Equals => "Equals".to_owned(),
        MetaType::FatArrow => "FatArrow".to_owned(),
        MetaType::SlottedProduct(elements) => format_meta_elements(elements, "; "),
        MetaType::Product(elements) => format_meta_elements(elements, ", "),
        MetaType::Optional(element) => format!("Option<{}>", format_meta_type(element)),
        MetaType::Sequence(element) => format!("Sequence<{}>", format_meta_type(element)),
        MetaType::Nominal(name, arguments) => {
            let arguments = arguments
                .iter()
                .map(format_meta_type)
                .collect::<Vec<_>>()
                .join(", ");
            if arguments.is_empty() {
                name.clone()
            } else {
                format!("{name}<{arguments}>")
            }
        }
        MetaType::Delimited(kind, contents) => {
            let name = match kind {
                DelimiterKind::Parenthesized => "Parenthesized",
                DelimiterKind::Bracketed => "Bracketed",
                DelimiterKind::Angled => "Angled",
                DelimiterKind::Braced => "Braced",
            };
            let contents = match contents {
                DelimitedMetaContents::Slotted(elements) if elements.len() == 1 => {
                    format_meta_type(&elements[0])
                }
                DelimitedMetaContents::Slotted(elements) => format_meta_elements(elements, "; "),
                DelimitedMetaContents::Product(elements) => format_meta_elements(elements, ", "),
                DelimitedMetaContents::Sequence(element) => {
                    format!("Sequence<{}>", format_meta_type(element))
                }
                DelimitedMetaContents::Separated { element, separator } => format!(
                    "Separated<{}, {}>",
                    format_meta_type(element),
                    format_meta_type(separator)
                ),
            };
            format!("{name}<{contents}>")
        }
    }
}

fn format_meta_elements(elements: &[MetaType], separator: &str) -> String {
    format!(
        "({})",
        elements
            .iter()
            .map(format_meta_type)
            .collect::<Vec<_>>()
            .join(separator)
    )
}

fn compile_type(ty: &Type) -> CompileType {
    // A bare product is a value type even when every element is a syntax
    // category: `(Expr; Expr)` is a slotted value, not one macro parameter
    // shape. Macro signatures read product shapes through `meta_type`
    // directly, so they are unaffected.
    if let Some(meta) = meta_type(ty)
        && !matches!(ty, Type::Product(_))
    {
        return CompileType::Meta(meta);
    }
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "Never" => CompileType::Never,
            "String" => CompileType::String,
            "Integer" | "I8" | "I16" | "I32" | "I64" | "ISize" | "U8" | "U16" | "U32" | "U64"
            | "USize" => CompileType::Integer,
            "Float" | "F32" | "F64" => CompileType::Float,
            "Bool" => CompileType::Bool,
            "ParseStream" => CompileType::Stream,
            name => CompileType::Named(name.to_owned()),
        },
        Type::NumberLiteral(_) => CompileType::Integer,
        Type::StringLiteral(_) => CompileType::String,
        Type::Inferred(_) | Type::Splice(_) => CompileType::Unknown,
        Type::Forall(_) => CompileType::Error,
        Type::Product(product) => {
            let elements = product
                .elements
                .iter()
                .map(|element| compile_type(&element.ty))
                .collect();
            if product.slotted {
                CompileType::SlottedProduct(elements)
            } else {
                CompileType::Product(elements)
            }
        }
        Type::Function(function) => CompileType::Function(
            Box::new(compile_type(&function.parameter)),
            Box::new(compile_type(&function.result)),
        ),
        Type::Application(application) => {
            // `Result<T, E>` is `Ok<T> | E`, whose two sides are tracked apart.
            if let Type::Application(inner) = application.callee.as_ref()
                && let Type::Named(head) = inner.callee.as_ref()
                && head.namespace.is_none()
                && head.name == "Result"
            {
                return CompileType::Result(
                    Box::new(compile_type(unwrap_singleton_product(&inner.argument))),
                    Box::new(compile_type(unwrap_singleton_product(
                        &application.argument,
                    ))),
                );
            }
            CompileType::Written(Box::new(ty.clone()))
        }
        Type::Sum(_) | Type::EffectApplication(_) | Type::Array(_) => {
            CompileType::Written(Box::new(ty.clone()))
        }
    }
}

fn compile_pattern_annotation(pattern: &Pattern) -> Option<CompileType> {
    match pattern {
        Pattern::Binding(binding) if !matches!(binding.ty, Type::Inferred(_)) => {
            Some(compile_type(&binding.ty))
        }
        Pattern::Wildcard(wildcard) if !matches!(wildcard.ty, Type::Inferred(_)) => {
            Some(compile_type(&wildcard.ty))
        }
        Pattern::At(at) if !matches!(at.binding.ty, Type::Inferred(_)) => {
            Some(compile_type(&at.binding.ty))
        }
        Pattern::At(at) => compile_pattern_annotation(&at.pattern),
        Pattern::Product(product) => Some(if product.slotted {
            CompileType::SlottedProduct(
                product
                    .elements
                    .iter()
                    .map(|element| {
                        compile_pattern_annotation(element).unwrap_or(CompileType::Unknown)
                    })
                    .collect(),
            )
        } else {
            CompileType::Product(
                product
                    .elements
                    .iter()
                    .map(|element| {
                        compile_pattern_annotation(element).unwrap_or(CompileType::Unknown)
                    })
                    .collect(),
            )
        }),
        _ => None,
    }
}

/// A compile-time binding's declared type: its annotation, or for a
/// value-defined function its parameter patterns' types, effect lower bounds,
/// and innermost `T :: …` result ascription.
fn binding_signature(binding: &Binding) -> Option<Type> {
    binding
        .annotation_body()
        .cloned()
        .or_else(|| binding.value.as_ref().and_then(function_value_signature))
}

pub(crate) fn function_value_signature(expression: &Expression) -> Option<Type> {
    let Expression::Function(function) = expression else {
        return None;
    };
    let result = match function.body.as_ref() {
        body @ Expression::Function(_) => function_value_signature(body)?,
        Expression::TypeAscription(ascription) => ascription.ty.clone(),
        _ => Type::Inferred(staple_syntax::InferredType::new()),
    };
    Some(Type::Function(staple_syntax::FunctionType {
        syntax: function.syntax.clone(),
        parameter: Box::new(function.pattern.ty()),
        mutable_marker: None,
        moved_marker: None,
        effects: function
            .effect_lower_bound
            .clone()
            .unwrap_or_else(staple_syntax::EffectSet::empty),
        result: Box::new(result),
    }))
}

fn helper_compile_type(binding: &Binding) -> CompileType {
    binding_signature(binding)
        .as_ref()
        .map(compile_type)
        .or_else(|| binding.value.as_ref().and_then(infer_compile_function_type))
        .unwrap_or(CompileType::Unknown)
}

fn infer_compile_function_type(expression: &Expression) -> Option<CompileType> {
    let Expression::Function(function) = expression else {
        return None;
    };
    let parameter = compile_pattern_annotation(&function.pattern).unwrap_or(CompileType::Unknown);
    let result = infer_compile_function_type(&function.body).unwrap_or(CompileType::Unknown);
    Some(CompileType::Function(Box::new(parameter), Box::new(result)))
}

fn compile_builtin_name_type(name: &str) -> Option<CompileType> {
    Some(match name {
        "Private" | "Package" | "Public" | "PublicReprPackage" | "PublicRepr" => {
            CompileType::Meta(MetaType::Visibility)
        }
        "Comma" => CompileType::Meta(MetaType::Comma),
        "Equals" => CompileType::Meta(MetaType::Equals),
        "FatArrow" => CompileType::Meta(MetaType::FatArrow),
        "True" | "False" => CompileType::Bool,
        "None" => CompileType::Named("None".to_owned()),
        name if name.chars().next().is_some_and(char::is_uppercase) => {
            CompileType::Named(name.to_owned())
        }
        _ => return None,
    })
}

fn compile_constructor_type(name: &str, argument: CompileType) -> CompileType {
    match name {
        "Ident" => CompileType::Meta(MetaType::Ident(None)),
        "CallExpr" => CompileType::Meta(MetaType::CallExpr),
        "CharExpr" => CompileType::Meta(MetaType::CharExpr),
        "StringExpr" => CompileType::Meta(MetaType::StringExpr),
        "BindingPattern" => CompileType::Meta(MetaType::BindingPattern),
        "NominalPattern" => CompileType::Meta(MetaType::NominalPattern),
        "ModifierCall" => CompileType::Meta(MetaType::ModifierCall),
        "UnstructuredModifier" => CompileType::Meta(MetaType::UnstructuredModifier),
        "ModifiedItem" => CompileType::Meta(MetaType::ModifiedItem),
        "TypeDeclarationItem" => CompileType::Meta(MetaType::TypeDeclarationItem),
        "TypeBody" => CompileType::Meta(MetaType::TypeBody),
        "TypeParameter" => CompileType::Meta(MetaType::TypeParameter),
        "TypeParameters" => CompileType::Meta(MetaType::TypeParameters),
        "ResourceDependency" => CompileType::Meta(MetaType::ResourceDependency),
        "UnstructuredItem" => CompileType::Meta(MetaType::UnstructuredItem),
        "Parenthesized" => CompileType::Meta(MetaType::Delimited(
            DelimiterKind::Parenthesized,
            compile_delimited_contents(&argument),
        )),
        "Bracketed" => CompileType::Meta(MetaType::Delimited(
            DelimiterKind::Bracketed,
            compile_delimited_contents(&argument),
        )),
        "Angled" => CompileType::Meta(MetaType::Delimited(
            DelimiterKind::Angled,
            compile_delimited_contents(&argument),
        )),
        "Braced" => CompileType::Meta(MetaType::Delimited(
            DelimiterKind::Braced,
            compile_delimited_contents(&argument),
        )),
        "Sequence" => match argument {
            CompileType::Product(elements) if elements.is_empty() => {
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::SyntaxNode)))
            }
            CompileType::Product(mut elements) if !elements.is_empty() => {
                let element = elements.remove(0);
                let meta = compile_type_as_meta(&element).unwrap_or(MetaType::SyntaxNode);
                CompileType::Meta(MetaType::Sequence(Box::new(meta)))
            }
            _ => CompileType::Meta(MetaType::Sequence(Box::new(MetaType::SyntaxNode))),
        },
        "Some" => match compile_type_as_meta(&argument) {
            Some(meta) => CompileType::Meta(MetaType::Optional(Box::new(meta))),
            None => CompileType::Named("Option".to_owned()),
        },
        "None" => CompileType::Named("Option".to_owned()),
        "Ok" => CompileType::Result(
            Box::new(match argument {
                CompileType::Product(mut elements) if elements.len() == 1 => elements.remove(0),
                other => other,
            }),
            Box::new(CompileType::Unknown),
        ),
        "True" | "False" => CompileType::Bool,
        _ => CompileType::Named(name.to_owned()),
    }
}

fn compile_delimited_contents(argument: &CompileType) -> DelimitedMetaContents {
    match argument {
        CompileType::Product(elements) | CompileType::SlottedProduct(elements) => {
            DelimitedMetaContents::Slotted(
                elements
                    .iter()
                    .map(|element| compile_type_as_meta(element).unwrap_or(MetaType::SyntaxNode))
                    .collect(),
            )
        }
        _ => DelimitedMetaContents::Slotted(vec![
            compile_type_as_meta(argument).unwrap_or(MetaType::SyntaxNode),
        ]),
    }
}

fn compile_type_as_meta(ty: &CompileType) -> Option<MetaType> {
    match ty {
        CompileType::Meta(meta) => Some(meta.clone()),
        CompileType::Product(elements) | CompileType::SlottedProduct(elements) => elements
            .iter()
            .map(compile_type_as_meta)
            .collect::<Option<Vec<_>>>()
            .map(MetaType::SlottedProduct),
        _ => None,
    }
}

fn inferred_compile_quote_type(quote: &staple_syntax::QuoteExpression) -> CompileType {
    CompileType::Meta(match &quote.template {
        staple_syntax::QuoteTemplate::Expression(_) => MetaType::Expr,
        staple_syntax::QuoteTemplate::Item(_) => MetaType::Item,
        staple_syntax::QuoteTemplate::Items(_) => MetaType::Sequence(Box::new(MetaType::Item)),
        staple_syntax::QuoteTemplate::Raw => MetaType::Syntax,
    })
}

fn compile_types_compatible(actual: &CompileType, expected: &CompileType) -> bool {
    if let (CompileType::Result(ao, ae), CompileType::Result(eo, ee)) = (actual, expected) {
        return compile_types_compatible(ao, eo) && compile_types_compatible(ae, ee);
    }
    if matches!(
        actual,
        CompileType::Error | CompileType::Unknown | CompileType::Never
    ) || matches!(expected, CompileType::Error | CompileType::Unknown)
        || compile_type_contains_unknown(actual)
        || compile_type_contains_unknown(expected)
    {
        return true;
    }
    if format_compile_type(actual) == format_compile_type(expected) {
        return true;
    }
    // The same slotted product may be represented as a value type or as a
    // syntax shape, depending on where the annotation came from.
    if let (Some(actual), Some(expected)) = (
        compile_function_slots(actual),
        compile_function_slots(expected),
    ) {
        return actual.len() == expected.len()
            && actual
                .iter()
                .zip(&expected)
                .all(|(a, e)| compile_types_compatible(a, e));
    }
    match (actual, expected) {
        (CompileType::Meta(actual), CompileType::Meta(expected)) => {
            actual == expected
                || unslotted_meta_type(actual) == unslotted_meta_type(expected)
                || matches!((actual, expected), (MetaType::Sequence(actual), MetaType::Sequence(expected))
                    if **actual == MetaType::SyntaxNode || **expected == MetaType::SyntaxNode || actual == expected)
                || *expected == MetaType::SyntaxNode
                || (*expected == MetaType::Expr && actual.is_expression())
                || (*expected == MetaType::Modifier
                    && matches!(
                        actual,
                        MetaType::ModifierCall | MetaType::UnstructuredModifier
                    ))
                || (*expected == MetaType::Item
                    && matches!(
                        actual,
                        MetaType::ModifiedItem
                            | MetaType::TypeDeclarationItem
                            | MetaType::UnstructuredItem
                    ))
                || (*expected == MetaType::Pattern
                    && matches!(actual, MetaType::BindingPattern | MetaType::NominalPattern))
                || matches!((actual, expected), (MetaType::Optional(actual), MetaType::Optional(expected))
                if compile_types_compatible(
                    &CompileType::Meta(actual.as_ref().clone()),
                    &CompileType::Meta(expected.as_ref().clone()),
                ))
        }
        // `None`, or a `Some` whose payload is not syntax, is still an `Option`.
        (CompileType::Named(actual), CompileType::Meta(MetaType::Optional(_))) => {
            actual == "Option"
        }
        (CompileType::Product(actual), CompileType::Product(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(a, e)| compile_types_compatible(a, e))
        }
        (CompileType::Function(ap, ar), CompileType::Function(ep, er)) => {
            compile_types_compatible(ap, ep) && compile_types_compatible(ar, er)
        }
        (CompileType::Named(actual), CompileType::Named(expected)) => actual == expected,
        _ => actual == expected,
    }
}

/// Compile-time product values don't record whether they are slotted, so a
/// constructed value is compared against its declared shape with slotted and
/// comma-separated products treated alike.
fn unslotted_meta_type(meta: &MetaType) -> MetaType {
    let elements = |elements: &[MetaType]| elements.iter().map(unslotted_meta_type).collect();
    match meta {
        MetaType::SlottedProduct(values) | MetaType::Product(values) => {
            MetaType::Product(elements(values))
        }
        MetaType::Optional(element) => MetaType::Optional(Box::new(unslotted_meta_type(element))),
        MetaType::Sequence(element) => MetaType::Sequence(Box::new(unslotted_meta_type(element))),
        MetaType::Delimited(kind, contents) => MetaType::Delimited(
            *kind,
            match contents {
                DelimitedMetaContents::Slotted(values) | DelimitedMetaContents::Product(values) => {
                    DelimitedMetaContents::Product(elements(values))
                }
                DelimitedMetaContents::Sequence(element) => {
                    DelimitedMetaContents::Sequence(Box::new(unslotted_meta_type(element)))
                }
                DelimitedMetaContents::Separated { element, separator } => {
                    DelimitedMetaContents::Separated {
                        element: Box::new(unslotted_meta_type(element)),
                        separator: Box::new(unslotted_meta_type(separator)),
                    }
                }
            },
        ),
        meta => meta.clone(),
    }
}

fn compile_type_contains_unknown(ty: &CompileType) -> bool {
    match ty {
        CompileType::Unknown => true,
        CompileType::Product(elements) | CompileType::SlottedProduct(elements) => {
            elements.iter().any(compile_type_contains_unknown)
        }
        CompileType::Function(parameter, result) => {
            compile_type_contains_unknown(parameter) || compile_type_contains_unknown(result)
        }
        CompileType::Result(ok, err) => {
            compile_type_contains_unknown(ok) || compile_type_contains_unknown(err)
        }
        _ => false,
    }
}

fn join_compile_types(left: CompileType, right: CompileType) -> CompileType {
    if left == CompileType::Never {
        return right;
    }
    if right == CompileType::Never {
        return left;
    }
    if left == CompileType::Error || right == CompileType::Error {
        return CompileType::Error;
    }
    if compile_types_compatible(&left, &right) {
        left
    } else {
        CompileType::Unknown
    }
}

fn format_compile_type(ty: &CompileType) -> String {
    match ty {
        CompileType::Error => "<error>".to_owned(),
        CompileType::Unknown => "_".to_owned(),
        CompileType::Never => "Never".to_owned(),
        CompileType::String => "String".to_owned(),
        CompileType::Integer => "Integer".to_owned(),
        CompileType::Float => "Float".to_owned(),
        CompileType::Bool => "Bool".to_owned(),
        CompileType::Named(name) => name.clone(),
        CompileType::Meta(meta) => format_meta_type(meta),
        CompileType::Stream => "ParseStream".to_owned(),
        CompileType::Written(ty) => ty.to_string(),
        CompileType::Result(ok, err) => {
            let argument = |ty: &CompileType| {
                let text = format_compile_type(ty);
                if text.contains(' ') && !text.starts_with('(') {
                    format!("({text})")
                } else {
                    text
                }
            };
            format!("Result {} {}", argument(ok), argument(err))
        }
        CompileType::Product(elements) => format!(
            "({})",
            elements
                .iter()
                .map(format_compile_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        CompileType::SlottedProduct(elements) => format!(
            "({})",
            elements
                .iter()
                .map(format_compile_type)
                .collect::<Vec<_>>()
                .join("; ")
        ),
        CompileType::Function(parameter, result) => format!(
            "{} -> {}",
            format_compile_type(parameter),
            format_compile_type(result)
        ),
    }
}

fn accessor_name(accessor: &Accessor) -> String {
    match accessor {
        Accessor::Name(name) | Accessor::Index(name) | Accessor::Method(name) => name.clone(),
        Accessor::Representation => "representation".to_owned(),
    }
}

/// The individual slots of a function parameter, when it is a slotted
/// product. Compile-time functions with such a parameter are curried by
/// juxtaposed calls, matching runtime functions.
fn compile_function_slots(parameter: &CompileType) -> Option<Vec<CompileType>> {
    match parameter {
        CompileType::SlottedProduct(elements) => Some(elements.clone()),
        CompileType::Meta(MetaType::SlottedProduct(elements)) => {
            Some(elements.iter().cloned().map(CompileType::Meta).collect())
        }
        _ => None,
    }
}

/// The function type left after `slots` have been filled. A partially applied
/// slotted function is still a function value.
fn compile_function_after_slots(mut slots: Vec<CompileType>, result: CompileType) -> CompileType {
    match slots.len() {
        0 => result,
        1 => CompileType::Function(Box::new(slots.remove(0)), Box::new(result)),
        _ => CompileType::Function(
            Box::new(CompileType::SlottedProduct(slots)),
            Box::new(result),
        ),
    }
}

/// Whether one argument supplies a whole slotted product for `slots`.
/// Only a syntactically slotted product literal or a binding of a slotted
/// product type fills every slot at once. If the first slot is itself a
/// product of the same arity, the argument may be filling that one slot
/// instead, so keep the ordinary one-slot interpretation.
fn compile_argument_fills_slots(
    expression: &Expression,
    environment: &CompileEnvironment,
    slots: &[CompileType],
) -> bool {
    let count = slots.len();
    let fills_whole = match ungroup_expression(expression) {
        Expression::Product(product) => {
            product.slotted
                && product.elements.len() == count
                && product.elements.iter().all(|element| !element.spread)
        }
        Expression::Name(name) => environment.get(&name.name).is_some_and(|binding| {
            matches!(&binding.ty, CompileType::SlottedProduct(elements) if elements.len() == count)
        }),
        _ => false,
    };
    let first_slot_is_product = match slots.first() {
        Some(CompileType::Product(elements) | CompileType::SlottedProduct(elements)) => {
            elements.len() == count
        }
        Some(CompileType::Meta(
            MetaType::Product(elements) | MetaType::SlottedProduct(elements),
        )) => elements.len() == count,
        _ => false,
    };
    fills_whole && !first_slot_is_product
}

/// A parenthesized single element stands for the element itself.
fn ungroup_expression(expression: &Expression) -> &Expression {
    let mut current = expression;
    while let Expression::Product(product) = current {
        let [element] = product.elements.as_slice() else {
            break;
        };
        if product.slotted || element.name.is_some() || element.spread || element.named_spread {
            break;
        }
        current = &element.value;
    }
    current
}

fn compile_field_type(ty: &CompileType, accessor: &Accessor) -> Option<CompileType> {
    let Accessor::Name(name) = accessor else {
        return None;
    };
    if let CompileType::Written(ty) = ty
        && let Some((element, separator)) = separated_meta_type(ty)
    {
        return match name.as_str() {
            "elements" => Some(CompileType::Meta(MetaType::Sequence(Box::new(element)))),
            "separator" => Some(CompileType::Meta(separator)),
            "trailing" => Some(CompileType::Bool),
            _ => None,
        };
    }
    if let CompileType::Written(ty) = ty
        && type_contains_named(ty, "Separated")
    {
        return match name.as_str() {
            "elements" | "separator" => Some(CompileType::Unknown),
            "trailing" => Some(CompileType::Bool),
            _ => None,
        };
    }
    match (ty, name.as_str()) {
        (CompileType::Meta(MetaType::TypeParameters), "parameters") => Some(CompileType::Meta(
            MetaType::Sequence(Box::new(MetaType::TypeParameter)),
        )),
        (CompileType::Meta(MetaType::TypeParameter), "name") => {
            Some(CompileType::Meta(MetaType::Ident(None)))
        }
        (CompileType::Meta(MetaType::TypeParameter), "default") => Some(CompileType::Meta(
            MetaType::Optional(Box::new(MetaType::Type)),
        )),
        (CompileType::Meta(MetaType::TypeParameter), "argument")
        | (CompileType::Meta(MetaType::ResourceDependency), "resource_type") => {
            Some(CompileType::Meta(MetaType::Type))
        }
        (CompileType::Meta(MetaType::ResourceDependency), "mutable") => Some(CompileType::Bool),
        (CompileType::Meta(MetaType::TypeParameter | MetaType::TypeParameters), "declaration")
        | (CompileType::Meta(MetaType::TypeParameters), "arguments" | "bounds") => {
            Some(CompileType::Meta(MetaType::Syntax))
        }

        (CompileType::Meta(MetaType::ModifierCall), "namespace") => Some(CompileType::Meta(
            MetaType::Sequence(Box::new(MetaType::Ident(None))),
        )),
        (CompileType::Meta(MetaType::ModifierCall), "name") => {
            Some(CompileType::Meta(MetaType::Ident(None)))
        }
        (CompileType::Meta(MetaType::ModifierCall), "argument") => Some(CompileType::Meta(
            MetaType::Optional(Box::new(MetaType::Syntax)),
        )),
        (CompileType::Meta(MetaType::CallExpr), "callee" | "argument") => {
            Some(CompileType::Meta(MetaType::Expr))
        }
        (CompileType::Meta(MetaType::MacroCallMetadata), "modifiers") => Some(CompileType::Meta(
            MetaType::Sequence(Box::new(MetaType::Modifier)),
        )),
        (CompileType::Meta(MetaType::MacroCallMetadata), "visibility") => {
            Some(CompileType::Meta(MetaType::Visibility))
        }
        _ => None,
    }
}

fn compile_product_fields(ty: &CompileType, length: usize) -> Vec<CompileType> {
    match ty {
        CompileType::Product(elements) | CompileType::SlottedProduct(elements) => elements.clone(),
        CompileType::Meta(MetaType::SlottedProduct(elements) | MetaType::Product(elements)) => {
            elements.iter().cloned().map(CompileType::Meta).collect()
        }
        CompileType::Meta(MetaType::MacroCallMetadata) if length == 2 => vec![
            CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Modifier))),
            CompileType::Meta(MetaType::Visibility),
        ],
        CompileType::Meta(MetaType::Sequence(element)) if length == 2 => vec![
            CompileType::Meta(element.as_ref().clone()),
            CompileType::Meta(MetaType::Sequence(element.clone())),
        ],
        _ => vec![CompileType::Unknown; length],
    }
}

fn compile_constructor_representation(ty: &CompileType, name: &str) -> Option<CompileType> {
    match (ty, name) {
        (CompileType::Meta(MetaType::Nominal(kind, arguments)), "Separated") if kind == "Separated" && arguments.len() == 2 => {
            Some(CompileType::Product(vec![CompileType::Meta(arguments[1].clone()), CompileType::Meta(MetaType::Sequence(Box::new(arguments[0].clone()))), CompileType::Bool]))
        },
        (CompileType::Meta(MetaType::TypeParameter), "TypeParameter") => {
            Some(CompileType::Product(vec![
                CompileType::Meta(MetaType::Ident(None)),
                CompileType::Meta(MetaType::Optional(Box::new(MetaType::Type))),
                CompileType::Meta(MetaType::Syntax),
                CompileType::Meta(MetaType::Type),
            ]))
        }
        (CompileType::Meta(MetaType::ResourceDependency), "ResourceDependency") => Some(
            CompileType::Product(vec![CompileType::Meta(MetaType::Type), CompileType::Bool]),
        ),
        (CompileType::Meta(MetaType::TypeParameters), "TypeParameters") => {
            Some(CompileType::Product(vec![
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::TypeParameter))),
                CompileType::Meta(MetaType::Syntax),
                CompileType::Meta(MetaType::Syntax),
                CompileType::Meta(MetaType::Syntax),
            ]))
        }
        (CompileType::Result(ok, _), "Ok") => Some(ok.as_ref().clone()),
        (CompileType::Result(_, error), "ParseError") if matches!(error.as_ref(), CompileType::Named(name) if name == "ParseError") => {
            Some(CompileType::Product(vec![CompileType::String]))
        }
        (CompileType::Named(named), "ParseError") if named == "ParseError" => {
            Some(CompileType::Product(vec![CompileType::String]))
        }
        (CompileType::Meta(MetaType::Optional(element)), "Some") => {
            Some(CompileType::Meta(element.as_ref().clone()))
        }
        (CompileType::Meta(MetaType::Optional(_)), "None") => {
            Some(CompileType::Product(Vec::new()))
        }
        (CompileType::Meta(MetaType::Sequence(element)), "Sequence") => {
            Some(CompileType::Product(vec![
                CompileType::Meta(element.as_ref().clone()),
                CompileType::Meta(MetaType::Sequence(element.clone())),
            ]))
        }
        (CompileType::Meta(MetaType::Modifier | MetaType::ModifierCall), "ModifierCall") => {
            Some(CompileType::Product(modifier_call_field_types()))
        }
        (
            CompileType::Meta(MetaType::Modifier | MetaType::UnstructuredModifier),
            "UnstructuredModifier",
        ) => Some(CompileType::Product(Vec::new())),
        (CompileType::Meta(MetaType::ModifiedItem), "ModifiedItem")
        | (CompileType::Meta(MetaType::Item), "ModifiedItem") => Some(CompileType::Product(vec![
            CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Modifier))),
            CompileType::Meta(MetaType::Item),
        ])),
        (CompileType::Meta(MetaType::TypeDeclarationItem), "TypeDeclarationItem")
        | (CompileType::Meta(MetaType::Item), "TypeDeclarationItem") => {
            Some(CompileType::Product(vec![
                CompileType::Meta(MetaType::Ident(None)),
                CompileType::Meta(MetaType::Type),
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Ident(None)))),
                CompileType::Meta(MetaType::Optional(Box::new(MetaType::TypeBody))),
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::ResourceDependency))),
            ]))
        }
        (CompileType::Meta(MetaType::TypeBody), "AliasBody") => {
            Some(CompileType::Meta(MetaType::Type))
        }
        (CompileType::Meta(MetaType::TypeBody), "WrapperBody" | "FromBody") => {
            Some(CompileType::Product(vec![
                CompileType::Meta(MetaType::Visibility),
                CompileType::Meta(MetaType::Type),
            ]))
        }
        (CompileType::Meta(MetaType::TypeBody), "OpaqueBody") => {
            Some(CompileType::Product(Vec::new()))
        }
        (CompileType::Meta(MetaType::CharExpr), "CharExpr")
        | (CompileType::Meta(MetaType::Expr), "CharExpr") => {
            Some(CompileType::Named("Char".to_owned()))
        }
        (CompileType::Meta(MetaType::Ident(_)), "Ident") => Some(CompileType::String),
        (CompileType::Meta(MetaType::CallExpr), "CallExpr") => Some(CompileType::Product(vec![
            CompileType::Meta(MetaType::Expr),
            CompileType::Meta(MetaType::Expr),
        ])),
        _ => None,
    }
}

fn compile_coverage_pattern(pattern: &Pattern, ty: &CompileType) -> CompileCoveragePattern {
    if let Pattern::Product(product) = pattern
        && product.elements.len() == 1
        && compile_product_fields(ty, 2).len() != 1
    {
        return compile_coverage_pattern(&product.elements[0], ty);
    }
    match pattern {
        Pattern::At(at) => compile_coverage_pattern(&at.pattern, ty),
        Pattern::Wildcard(_) => CompileCoveragePattern::Any,
        Pattern::Binding(binding)
            if !binding.name.chars().next().is_some_and(char::is_uppercase) =>
        {
            CompileCoveragePattern::Any
        }
        Pattern::Binding(binding) => {
            CompileCoveragePattern::Constructor(binding.name.clone(), Vec::new())
        }
        Pattern::StringLiteral(pattern) => CompileCoveragePattern::Literal(
            staple_syntax::string_literal::decode(&pattern.literal)
                .unwrap_or_else(|_| pattern.literal.clone()),
        ),
        Pattern::Product(product) => CompileCoveragePattern::Constructor(
            "$product".to_owned(),
            product
                .elements
                .iter()
                .zip(compile_product_fields(ty, product.elements.len()))
                .map(|(pattern, ty)| compile_coverage_pattern(pattern, &ty))
                .collect(),
        ),
        Pattern::Nominal(nominal) if nominal.name == "Sequence" => {
            compile_sequence_coverage_pattern(&nominal.argument, ty)
        }
        Pattern::Nominal(nominal) => {
            let representation = compile_constructor_representation(ty, &nominal.name)
                .unwrap_or(CompileType::Unknown);
            let argument = compile_coverage_pattern(&nominal.argument, &representation);
            // A constructor whose payload is itself a product keeps that product
            // as one argument; the coverage recursion then specializes its
            // fields. Flattening it would disagree with the single payload
            // argument `compile_type_constructors` reports.
            let arguments = match (&representation, argument) {
                (CompileType::Meta(MetaType::SlottedProduct(_) | MetaType::Product(_)), other) => {
                    vec![other]
                }
                (_, CompileCoveragePattern::Constructor(name, arguments)) if name == "$product" => {
                    arguments
                }
                (_, other) => vec![other],
            };
            CompileCoveragePattern::Constructor(nominal.name.clone(), arguments)
        }
        Pattern::Splice(_) => CompileCoveragePattern::Invalid,
    }
}

fn compile_pattern_is_catch_all(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wildcard(_) => true,
        Pattern::At(at) => compile_pattern_is_catch_all(&at.pattern),
        Pattern::Binding(binding) => {
            !binding.name.chars().next().is_some_and(char::is_uppercase)
                && matches!(binding.ty, Type::Inferred(_))
        }
        _ => false,
    }
}

fn compile_pattern_is_useful(previous: &[&Pattern], candidate: &Pattern) -> bool {
    if previous
        .iter()
        .any(|pattern| compile_pattern_is_catch_all(pattern))
    {
        return false;
    }
    !previous
        .iter()
        .any(|pattern| pattern.syntax().text() == candidate.syntax().text())
}

fn compile_patterns_are_exhaustive(ty: &CompileType, patterns: &[&Pattern]) -> bool {
    if patterns
        .iter()
        .any(|pattern| compile_pattern_covers_type(pattern, ty))
    {
        return true;
    }
    match ty {
        CompileType::Meta(MetaType::Nominal(name, _)) if name == "Separated" => compile_pattern_matrix_is_exhaustive(ty, patterns),
        CompileType::Bool => ["True", "False"]
            .iter()
            .all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name))),
        CompileType::Product(_)
        | CompileType::SlottedProduct(_)
        | CompileType::Meta(MetaType::SlottedProduct(_) | MetaType::Product(_)) => {
            compile_pattern_matrix_is_exhaustive(ty, patterns)
        }
        CompileType::Meta(MetaType::Sequence(_)) => {
            let empty = patterns.iter().any(|pattern| sequence_pattern_kind(pattern) == Some(false));
            let all_non_empty = patterns.iter().any(|pattern| sequence_pattern_kind(pattern) == Some(true));
            (empty && all_non_empty) || patterns.len() >= 2
        }
        CompileType::Meta(MetaType::Optional(_)) => {
            ["None", "Some"].iter().all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name)))
        }
        CompileType::Named(name) if name == "Option" => {
            ["None", "Some"].iter().all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name)))
        }
        CompileType::Result(_, error) => {
            let has = |name: &str| {
                patterns
                    .iter()
                    .any(|pattern| compile_pattern_constructor_name(pattern) == Some(name))
            };
            has("Ok")
                && match error.as_ref() {
                    CompileType::Named(name) => has(name),
                    _ => patterns.len() >= 2,
                }
        }
        CompileType::Meta(MetaType::Visibility) => ["Private", "Package", "Public"]
            .iter()
            .all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name))),
        CompileType::Meta(MetaType::Modifier) => ["ModifierCall", "UnstructuredModifier"].iter().all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name))),
        CompileType::Meta(MetaType::ModifierCall) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("ModifierCall")),
        CompileType::Meta(MetaType::UnstructuredModifier) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("UnstructuredModifier")),
        CompileType::Meta(MetaType::Item) => ["ModifiedItem", "TypeDeclarationItem", "UnstructuredItem"]
            .iter()
            .all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name))),
        CompileType::Meta(MetaType::Expr) => ["Ident", "CallExpr", "CharExpr", "StringExpr", "UnstructuredExpr"]
            .iter()
            .all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name))),
        CompileType::Meta(MetaType::Pattern) => ["BindingPattern", "NominalPattern", "UnstructuredPattern"]
            .iter()
            .all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name))),
        CompileType::Meta(MetaType::Delimited(kind, contents)) => {
            let expected = match kind {
                DelimiterKind::Parenthesized => "Parenthesized",
                DelimiterKind::Bracketed => "Bracketed",
                DelimiterKind::Angled => "Angled",
                DelimiterKind::Braced => "Braced",
            };
            patterns.iter().any(|pattern| {
                let Pattern::Nominal(nominal) = strip_at_pattern(pattern) else { return false; };
                let _ = contents;
                nominal.name == expected
            })
        }
        CompileType::Meta(MetaType::CharExpr) => patterns.iter().any(|pattern| {
            let Pattern::Nominal(nominal) = strip_at_pattern(pattern) else { return false; };
            nominal.name == "CharExpr" && compile_pattern_covers_type(&nominal.argument, &CompileType::Named("Char".to_owned()))
        }),
        CompileType::Meta(MetaType::CallExpr) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("CallExpr")),
        CompileType::Meta(MetaType::Ident(None)) => patterns.iter().any(|pattern| {
            let Pattern::Nominal(nominal) = strip_at_pattern(pattern) else { return false; };
            nominal.name == "Ident" && compile_pattern_is_catch_all(&nominal.argument)
        }),
        CompileType::Meta(MetaType::Ident(Some(spelling))) => patterns.iter().any(|pattern| {
            let Pattern::Nominal(nominal) = strip_at_pattern(pattern) else { return false; };
            nominal.name == "Ident" && matches!(nominal.argument.as_ref(), Pattern::StringLiteral(value) if staple_syntax::string_literal::decode(&value.literal).is_ok_and(|value| &value == spelling))
        }),
        CompileType::Meta(MetaType::ModifiedItem) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("ModifiedItem")),
        CompileType::Meta(MetaType::TypeParameter | MetaType::TypeParameters | MetaType::ResourceDependency) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(format_compile_type(ty).as_str())),
        CompileType::Meta(MetaType::TypeDeclarationItem) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("TypeDeclarationItem")),
        CompileType::Meta(MetaType::TypeBody) => ["AliasBody", "WrapperBody", "FromBody", "OpaqueBody"]
            .iter()
            .all(|name| patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some(*name))),
        CompileType::Meta(MetaType::UnstructuredItem) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("UnstructuredItem")),
        CompileType::Meta(MetaType::Comma) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("Comma")),
        CompileType::Meta(MetaType::Equals) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("Equals")),
        CompileType::Meta(MetaType::FatArrow) => patterns.iter().any(|pattern| compile_pattern_constructor_name(pattern) == Some("FatArrow")),
        CompileType::Meta(MetaType::MacroCallMetadata) => patterns.iter().any(|pattern| matches!(strip_at_pattern(pattern), Pattern::Product(_))),
        _ => false,
    }
}

fn compile_pattern_matrix_is_exhaustive(ty: &CompileType, patterns: &[&Pattern]) -> bool {
    let matrix = patterns
        .iter()
        .map(|pattern| vec![compile_coverage_pattern(pattern, ty)])
        .collect::<Vec<_>>();
    !compile_coverage_is_useful(
        std::slice::from_ref(ty),
        &matrix,
        &[CompileCoveragePattern::Any],
    )
}

fn compile_pattern_covers_type(pattern: &Pattern, ty: &CompileType) -> bool {
    match strip_at_pattern(pattern) {
        Pattern::Wildcard(wildcard) => {
            matches!(wildcard.ty, Type::Inferred(_))
                || compile_types_compatible(&compile_type(&wildcard.ty), ty)
        }
        Pattern::Binding(binding)
            if !binding.name.chars().next().is_some_and(char::is_uppercase) =>
        {
            matches!(binding.ty, Type::Inferred(_))
                || compile_types_compatible(&compile_type(&binding.ty), ty)
        }
        Pattern::Product(product) => {
            let fields = compile_product_fields(ty, product.elements.len());
            fields.len() == product.elements.len()
                && product
                    .elements
                    .iter()
                    .zip(fields)
                    .all(|(pattern, ty)| compile_pattern_covers_type(pattern, &ty))
        }
        _ => false,
    }
}

fn strip_at_pattern(mut pattern: &Pattern) -> &Pattern {
    while let Pattern::At(at) = pattern {
        pattern = &at.pattern;
    }
    pattern
}

fn strip_grouped_pattern(mut pattern: &Pattern) -> &Pattern {
    loop {
        pattern = strip_at_pattern(pattern);
        match pattern {
            Pattern::Product(product) if product.elements.len() == 1 => {
                pattern = &product.elements[0];
            }
            _ => return pattern,
        }
    }
}

fn compile_pattern_constructor_name(pattern: &Pattern) -> Option<&str> {
    match strip_at_pattern(pattern) {
        Pattern::Binding(binding)
            if binding.name.chars().next().is_some_and(char::is_uppercase) =>
        {
            Some(&binding.name)
        }
        Pattern::Nominal(nominal) => Some(&nominal.name),
        _ => None,
    }
}

/// `Some(true)` means a head/tail pattern covering every non-empty sequence;
/// `Some(false)` means the empty constructor.
fn sequence_pattern_kind(pattern: &Pattern) -> Option<bool> {
    let text = pattern.syntax().text();
    if text.trim_start().starts_with("Sequence") {
        if text.contains("first:") && text.contains("rest:") {
            return Some(true);
        }
        if text
            .split_whitespace()
            .collect::<String>()
            .starts_with("Sequence()")
        {
            return Some(false);
        }
    }
    let Pattern::Nominal(nominal) = strip_at_pattern(pattern) else {
        return None;
    };
    if nominal.name != "Sequence" {
        return None;
    }
    let Pattern::Product(product) = strip_grouped_pattern(&nominal.argument) else {
        return None;
    };
    if product.elements.is_empty() {
        return Some(false);
    }
    if product.elements.len() == 2 {
        return Some(true);
    }
    None
}

fn compile_sequence_coverage_pattern(
    pattern: &Pattern,
    ty: &CompileType,
) -> CompileCoveragePattern {
    let CompileType::Meta(MetaType::Sequence(element)) = ty else {
        return CompileCoveragePattern::Invalid;
    };
    let Pattern::Product(product) = pattern else {
        return CompileCoveragePattern::Invalid;
    };
    if product.elements.is_empty() {
        return CompileCoveragePattern::Constructor("Sequence.Empty".to_owned(), Vec::new());
    }
    if product.elements.len() == 2
        && matches!(&product.elements[0], Pattern::Binding(binding) if binding.name == "first")
        && matches!(&product.elements[1], Pattern::Binding(binding) if binding.name == "rest")
    {
        return CompileCoveragePattern::Constructor(
            "Sequence.Cons".to_owned(),
            vec![
                compile_coverage_pattern(
                    &product.elements[0],
                    &CompileType::Meta(element.as_ref().clone()),
                ),
                compile_coverage_pattern(&product.elements[1], ty),
            ],
        );
    }
    let mut tail = CompileCoveragePattern::Constructor("Sequence.Empty".to_owned(), Vec::new());
    for element_pattern in product.elements.iter().rev() {
        tail = CompileCoveragePattern::Constructor(
            "Sequence.Cons".to_owned(),
            vec![
                compile_coverage_pattern(
                    element_pattern,
                    &CompileType::Meta(element.as_ref().clone()),
                ),
                tail,
            ],
        );
    }
    tail
}

fn compile_type_constructors(ty: &CompileType) -> Option<Vec<(String, Vec<CompileType>)>> {
    Some(match ty {
        CompileType::Meta(MetaType::Nominal(name, _)) if name == "Separated" => {
            let CompileType::Product(fields) = compile_constructor_representation(ty, name)? else { return None; };
            vec![(name.clone(), fields)]
        },
        CompileType::Meta(
            MetaType::TypeParameter | MetaType::TypeParameters | MetaType::ResourceDependency,
        ) => {
            let name = format_compile_type(ty);
            let CompileType::Product(fields) = compile_constructor_representation(ty, &name)?
            else {
                return None;
            };
            vec![(name, fields)]
        }
        CompileType::Bool => vec![
            ("True".to_owned(), Vec::new()),
            ("False".to_owned(), Vec::new()),
        ],
        CompileType::Product(elements) | CompileType::SlottedProduct(elements) => {
            vec![("$product".to_owned(), elements.clone())]
        }
        CompileType::Meta(MetaType::SlottedProduct(elements) | MetaType::Product(elements)) => {
            vec![(
                "$product".to_owned(),
                elements.iter().cloned().map(CompileType::Meta).collect(),
            )]
        }
        CompileType::Meta(MetaType::Sequence(element)) => vec![
            ("Sequence.Empty".to_owned(), Vec::new()),
            (
                "Sequence.Cons".to_owned(),
                vec![
                    CompileType::Meta(element.as_ref().clone()),
                    CompileType::Meta(MetaType::Sequence(element.clone())),
                ],
            ),
        ],
        CompileType::Meta(MetaType::Optional(element)) => vec![
            ("None".to_owned(), Vec::new()),
            (
                "Some".to_owned(),
                vec![CompileType::Meta(element.as_ref().clone())],
            ),
        ],
        CompileType::Meta(MetaType::Visibility) => vec![
            ("Private".to_owned(), Vec::new()),
            ("Package".to_owned(), Vec::new()),
            ("Public".to_owned(), Vec::new()),
        ],
        CompileType::Meta(MetaType::Item) => vec![
            (
                "ModifiedItem".to_owned(),
                vec![
                    CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Modifier))),
                    CompileType::Meta(MetaType::Item),
                ],
            ),
            (
                "TypeDeclarationItem".to_owned(),
                vec![
                    CompileType::Meta(MetaType::Ident(None)),
                    CompileType::Meta(MetaType::Type),
                    CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Ident(None)))),
                    CompileType::Meta(MetaType::Optional(Box::new(MetaType::TypeBody))),
                    CompileType::Meta(MetaType::Sequence(Box::new(
                        MetaType::ResourceDependency,
                    ))),
                ],
            ),
            ("UnstructuredItem".to_owned(), Vec::new()),
        ],
        CompileType::Meta(MetaType::Modifier) => vec![
            ("ModifierCall".to_owned(), modifier_call_field_types()),
            ("UnstructuredModifier".to_owned(), Vec::new()),
        ],
        CompileType::Meta(MetaType::ModifierCall) => {
            vec![("ModifierCall".to_owned(), modifier_call_field_types())]
        }
        CompileType::Meta(MetaType::UnstructuredModifier) => {
            vec![("UnstructuredModifier".to_owned(), Vec::new())]
        }
        CompileType::Meta(MetaType::ModifiedItem) => vec![(
            "ModifiedItem".to_owned(),
            vec![
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Modifier))),
                CompileType::Meta(MetaType::Item),
            ],
        )],
        CompileType::Meta(MetaType::TypeDeclarationItem) => vec![(
            "TypeDeclarationItem".to_owned(),
            vec![
                CompileType::Meta(MetaType::Ident(None)),
                CompileType::Meta(MetaType::Type),
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Ident(None)))),
                CompileType::Meta(MetaType::Optional(Box::new(MetaType::TypeBody))),
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::ResourceDependency))),
            ],
        )],
        CompileType::Meta(MetaType::TypeBody) => vec![
            (
                "AliasBody".to_owned(),
                vec![CompileType::Meta(MetaType::Type)],
            ),
            (
                "WrapperBody".to_owned(),
                vec![
                    CompileType::Meta(MetaType::Visibility),
                    CompileType::Meta(MetaType::Type),
                ],
            ),
            (
                "FromBody".to_owned(),
                vec![
                    CompileType::Meta(MetaType::Visibility),
                    CompileType::Meta(MetaType::Type),
                ],
            ),
            ("OpaqueBody".to_owned(), Vec::new()),
        ],
        CompileType::Meta(MetaType::UnstructuredItem) => {
            vec![("UnstructuredItem".to_owned(), Vec::new())]
        }
        CompileType::Meta(MetaType::Ident(_)) => {
            vec![("Ident".to_owned(), vec![CompileType::String])]
        }
        CompileType::Meta(MetaType::CharExpr) => vec![(
            "CharExpr".to_owned(),
            vec![CompileType::Named("Char".to_owned())],
        )],
        CompileType::Meta(MetaType::CallExpr) => vec![(
            "CallExpr".to_owned(),
            vec![
                CompileType::Meta(MetaType::Expr),
                CompileType::Meta(MetaType::Expr),
            ],
        )],
        CompileType::Meta(MetaType::Expr) => vec![
            ("Ident".to_owned(), vec![CompileType::String]),
            (
                "CallExpr".to_owned(),
                vec![
                    CompileType::Meta(MetaType::Expr),
                    CompileType::Meta(MetaType::Expr),
                ],
            ),
            (
                "CharExpr".to_owned(),
                vec![CompileType::Named("Char".to_owned())],
            ),
            ("StringExpr".to_owned(), vec![CompileType::String]),
            ("UnstructuredExpr".to_owned(), Vec::new()),
        ],
        CompileType::Meta(MetaType::Pattern) => vec![
            (
                "BindingPattern".to_owned(),
                vec![CompileType::Meta(MetaType::Ident(None))],
            ),
            (
                "NominalPattern".to_owned(),
                vec![
                    CompileType::Meta(MetaType::Ident(None)),
                    CompileType::Meta(MetaType::Pattern),
                ],
            ),
            ("UnstructuredPattern".to_owned(), Vec::new()),
        ],
        CompileType::Meta(MetaType::Delimited(kind, contents)) => {
            let name = match kind {
                DelimiterKind::Parenthesized => "Parenthesized",
                DelimiterKind::Bracketed => "Bracketed",
                DelimiterKind::Angled => "Angled",
                DelimiterKind::Braced => "Braced",
            };
            let contents = match contents {
                DelimitedMetaContents::Slotted(elements)
                | DelimitedMetaContents::Product(elements) => {
                    CompileType::Product(elements.iter().cloned().map(CompileType::Meta).collect())
                }
                DelimitedMetaContents::Sequence(element) => {
                    CompileType::Meta(MetaType::Sequence(element.clone()))
                }
                DelimitedMetaContents::Separated { element, separator } => {
                    CompileType::Product(vec![
                        CompileType::Meta(separator.as_ref().clone()),
                        CompileType::Meta(MetaType::Sequence(element.clone())),
                        CompileType::Bool,
                    ])
                }
            };
            vec![(
                name.to_owned(),
                match contents {
                    CompileType::Product(elements) => elements,
                    other => vec![other],
                },
            )]
        }
        CompileType::Meta(MetaType::MacroCallMetadata) => vec![(
            "$product".to_owned(),
            vec![
                CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Modifier))),
                CompileType::Meta(MetaType::Visibility),
            ],
        )],
        CompileType::Meta(MetaType::Comma) => vec![("Comma".to_owned(), Vec::new())],
        CompileType::Meta(MetaType::Equals) => vec![("Equals".to_owned(), Vec::new())],
        CompileType::Meta(MetaType::FatArrow) => vec![("FatArrow".to_owned(), Vec::new())],
        _ => return None,
    })
}

fn compile_coverage_is_useful(
    types: &[CompileType],
    matrix: &[Vec<CompileCoveragePattern>],
    candidate: &[CompileCoveragePattern],
) -> bool {
    compile_coverage_is_useful_with_fuel(types, matrix, candidate, 256)
}

fn compile_coverage_is_useful_with_fuel(
    types: &[CompileType],
    matrix: &[Vec<CompileCoveragePattern>],
    candidate: &[CompileCoveragePattern],
    fuel: usize,
) -> bool {
    if fuel == 0 {
        // Recursive syntax types can describe an unbounded constructor
        // space. Conservatively retain the candidate rather than overflowing
        // while proving usefulness.
        return true;
    }
    if matrix.is_empty() {
        return true;
    }
    if matrix.iter().any(|row| {
        row.len() == candidate.len()
            && row
                .iter()
                .all(|pattern| matches!(pattern, CompileCoveragePattern::Any))
    }) {
        return false;
    }
    if types.is_empty() {
        return !matrix.iter().any(Vec::is_empty);
    }
    let first = &candidate[0];
    if matches!(first, CompileCoveragePattern::Any)
        && matrix
            .iter()
            .all(|row| matches!(row.first(), Some(CompileCoveragePattern::Any)))
    {
        let reduced_matrix = matrix
            .iter()
            .map(|row| row[1..].to_vec())
            .collect::<Vec<_>>();
        return compile_coverage_is_useful_with_fuel(
            &types[1..],
            &reduced_matrix,
            &candidate[1..],
            fuel - 1,
        );
    }
    if let Some(constructors) = compile_type_constructors(&types[0]) {
        let selected = match first {
            CompileCoveragePattern::Any => constructors,
            CompileCoveragePattern::Constructor(name, _) => constructors
                .into_iter()
                .filter(|(constructor, _)| constructor == name)
                .collect(),
            CompileCoveragePattern::Literal(_) | CompileCoveragePattern::Invalid => return false,
        };
        return selected.into_iter().any(|(name, arguments)| {
            let specialized_matrix = matrix
                .iter()
                .filter_map(|row| specialize_compile_coverage_row(row, &name, arguments.len()))
                .collect::<Vec<_>>();
            let Some(specialized_candidate) =
                specialize_compile_coverage_row(candidate, &name, arguments.len())
            else {
                return false;
            };
            let mut specialized_types = arguments;
            specialized_types.extend_from_slice(&types[1..]);
            compile_coverage_is_useful_with_fuel(
                &specialized_types,
                &specialized_matrix,
                &specialized_candidate,
                fuel - 1,
            )
        });
    }

    match first {
        CompileCoveragePattern::Literal(literal) => {
            let specialized_matrix = matrix
                .iter()
                .filter(|row| matches!(&row[0], CompileCoveragePattern::Any) || matches!(&row[0], CompileCoveragePattern::Literal(value) if value == literal))
                .map(|row| row[1..].to_vec())
                .collect::<Vec<_>>();
            compile_coverage_is_useful_with_fuel(
                &types[1..],
                &specialized_matrix,
                &candidate[1..],
                fuel - 1,
            )
        }
        CompileCoveragePattern::Any => {
            let specialized_matrix = matrix
                .iter()
                .filter(|row| matches!(row[0], CompileCoveragePattern::Any))
                .map(|row| row[1..].to_vec())
                .collect::<Vec<_>>();
            compile_coverage_is_useful_with_fuel(
                &types[1..],
                &specialized_matrix,
                &candidate[1..],
                fuel - 1,
            )
        }
        _ => false,
    }
}

fn specialize_compile_coverage_row(
    row: &[CompileCoveragePattern],
    constructor: &str,
    arity: usize,
) -> Option<Vec<CompileCoveragePattern>> {
    let mut result = match &row[0] {
        CompileCoveragePattern::Any => vec![CompileCoveragePattern::Any; arity],
        CompileCoveragePattern::Constructor(name, arguments) if name == constructor => {
            if arguments.len() == arity {
                arguments.clone()
            } else {
                return None;
            }
        }
        _ => return None,
    };
    result.extend_from_slice(&row[1..]);
    Some(result)
}

/// Reads a macro annotation's parameter list and result. Multiple
/// parameters are written juxtaposed (`(A; B) -> C`), a single
/// `Type::Function` whose parameter is a product; a curried chain of nested
/// `Type::Function`s (the removed `A -> B -> C` spelling) is rejected by
/// `meta_type` failing on the nested `Type::Function` result, same as any
/// other malformed annotation shape.
fn macro_signature(annotation: &Type) -> Option<(Vec<MetaType>, MetaType)> {
    let Type::Function(function) = annotation else {
        return None;
    };
    let parameters = juxtaposed_meta_types(function)?;
    let result = meta_type(&function.result)?;
    (!parameters.is_empty()).then_some((parameters, result))
}

/// Reads one function type's parameter(s) as meta types: a juxtaposed
/// parameter list becomes one entry per element, and a single parameter
/// (including a genuine, non-juxtaposed product) becomes one entry.
fn juxtaposed_meta_types(function: &staple_syntax::FunctionType) -> Option<Vec<MetaType>> {
    let product = match function.parameter.as_ref() {
        Type::Product(product) if product.slotted => product,
        _ => return Some(vec![meta_type(&function.parameter)?]),
    };
    product
        .elements
        .iter()
        .map(|element| {
            (!element.spread && !element.mutable && !element.moved)
                .then(|| meta_type(&element.ty))
                .flatten()
        })
        .collect()
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
        Expression::Quote(quote) if quote.kind == staple_syntax::QuoteKind::Quote => {
            MetaType::Syntax
        }
        Expression::Quote(quote) => match &quote.template {
            staple_syntax::QuoteTemplate::Expression(_) => MetaType::Expr,
            staple_syntax::QuoteTemplate::Item(_) => MetaType::Item,
            staple_syntax::QuoteTemplate::Items(_) => MetaType::Sequence(Box::new(MetaType::Item)),
            staple_syntax::QuoteTemplate::Raw => MetaType::Syntax,
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
        // A type ascription asserts the result type of the quoted
        // template, so infer from it directly when it names a meta type;
        // otherwise fall back to the annotated value's own shape.
        Expression::TypeAscription(ascription) => meta_type(&ascription.ty)
            .unwrap_or_else(|| inferred_result_meta_type(&ascription.value)),
        _ => MetaType::SyntaxNode,
    }
}

fn macro_body_parameter_types(expression: &Expression) -> Vec<Option<MetaType>> {
    let mut parameters = Vec::new();
    let mut current = expression;
    while let Expression::Function(function) = current {
        parameters.push(match &function.pattern {
            Pattern::Binding(binding) if matches!(binding.ty, Type::Inferred(_)) => None,
            // An unannotated element of a destructured product takes its type
            // from the annotation, so the product cannot conflict with it.
            Pattern::Product(product)
                if product.elements.iter().any(pattern_has_inferred_element) =>
            {
                None
            }
            pattern => pattern_meta_type(pattern),
        });
        current = &function.body;
    }
    parameters
}

fn pattern_has_inferred_element(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Binding(binding) => matches!(binding.ty, Type::Inferred(_)),
        Pattern::Wildcard(wildcard) => matches!(wildcard.ty, Type::Inferred(_)),
        Pattern::Product(product) => product.elements.iter().any(pattern_has_inferred_element),
        _ => false,
    }
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
        Pattern::Product(product) => {
            let elements = product
                .elements
                .iter()
                .map(pattern_meta_type)
                .collect::<Option<Vec<_>>>()?;
            Some(if product.slotted {
                MetaType::SlottedProduct(elements)
            } else {
                MetaType::Product(elements)
            })
        }
        Pattern::Nominal(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => None,
    }
}

fn type_contains_syntax(ty: &Type) -> bool {
    match ty {
        Type::Forall(ty) => forall_types(ty).any(type_contains_syntax),
        // `Trait.Item` is an associated-type projection, not the syntax type.
        Type::Named(named)
            if named
                .namespace
                .as_deref()
                .and_then(|namespace| namespace.rsplit('.').next())
                .is_some_and(|segment| segment.starts_with(char::is_uppercase)) =>
        {
            false
        }
        Type::Named(named) => matches!(
            named.name.as_str(),
            "Syntax"
                | "SyntaxNode"
                | "Expr"
                | "Ident"
                | "CallExpr"
                | "Sequence"
                | "Separated"
                | "Comma"
                | "Equals"
                | "FatArrow"
                | "Parenthesized"
                | "Bracketed"
                | "Angled"
                | "Braced"
                | "UnstructuredExpr"
                | "Type"
                | "Pattern"
                | "Item"
                | "Modifier"
                | "ModifierCall"
                | "UnstructuredModifier"
                | "ModifiedItem"
                | "Visibility"
                | "MacroCallMetadata"
                | "ParseStream"
                | "Private"
                | "Package"
                | "Public"
                | "PublicReprPackage"
                | "PublicRepr"
        ),
        Type::Function(function) => {
            type_contains_syntax(&function.parameter)
                || function
                    .effects
                    .resources
                    .iter()
                    .any(|resource| type_contains_syntax(&resource.value_type))
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
        Type::EffectApplication(application) => {
            type_contains_syntax(&application.callee)
                || application
                    .effects
                    .resources
                    .iter()
                    .any(|resource| type_contains_syntax(&resource.value_type))
        }
        Type::Array(array) => {
            type_contains_syntax(&array.element) || type_contains_syntax(&array.count)
        }
        Type::Inferred(_) | Type::NumberLiteral(_) | Type::StringLiteral(_) | Type::Splice(_) => {
            false
        }
    }
}

fn type_contains_unshadowed_syntax(ty: &Type, declared: &std::collections::HashSet<&str>) -> bool {
    match ty {
        Type::Forall(ty) => {
            let mut declared = declared.clone();
            for parameter in &ty.generics.parameters {
                declared.extend(parameter.names());
            }
            forall_types(ty).any(|ty| type_contains_unshadowed_syntax(ty, &declared))
        }
        Type::Named(named)
            if named.namespace.is_none() && declared.contains(named.name.as_str()) =>
        {
            false
        }
        Type::Named(_)
        | Type::NumberLiteral(_)
        | Type::StringLiteral(_)
        | Type::Inferred(_)
        | Type::Splice(_) => type_contains_syntax(ty),
        Type::Function(function) => {
            type_contains_unshadowed_syntax(&function.parameter, declared)
                || function
                    .effects
                    .resources
                    .iter()
                    .any(|resource| type_contains_unshadowed_syntax(&resource.value_type, declared))
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
        Type::EffectApplication(application) => {
            type_contains_unshadowed_syntax(&application.callee, declared)
                || application
                    .effects
                    .resources
                    .iter()
                    .any(|resource| type_contains_unshadowed_syntax(&resource.value_type, declared))
        }
        Type::Array(array) => {
            type_contains_unshadowed_syntax(&array.element, declared)
                || type_contains_unshadowed_syntax(&array.count, declared)
        }
    }
}

fn binding_is_compile_time_helper(binding: &Binding) -> bool {
    binding.kind == BindingKind::Def && binding_contains_syntax(binding)
}

fn binding_contains_syntax(binding: &Binding) -> bool {
    binding_signature(binding).is_some_and(|ty| type_contains_syntax(&ty))
        || binding
            .value
            .as_ref()
            .is_some_and(expression_parameter_contains_syntax)
}

fn binding_uses_macro_call_metadata(binding: &Binding) -> bool {
    binding_signature(binding)
        .is_some_and(|annotation| type_contains_named(&annotation, "MacroCallMetadata"))
        || binding.value.as_ref().is_some_and(|value| {
            let mut current = value;
            while let Expression::Function(function) = current {
                if pattern_uses_macro_call_metadata(&function.pattern) {
                    return true;
                }
                current = &function.body;
            }
            false
        })
}

fn pattern_uses_macro_call_metadata(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Binding(binding) => type_contains_named(&binding.ty, "MacroCallMetadata"),
        Pattern::Wildcard(wildcard) => type_contains_named(&wildcard.ty, "MacroCallMetadata"),
        Pattern::At(at) => {
            type_contains_named(&at.binding.ty, "MacroCallMetadata")
                || pattern_uses_macro_call_metadata(&at.pattern)
        }
        Pattern::Product(product) => product
            .elements
            .iter()
            .any(pattern_uses_macro_call_metadata),
        Pattern::Nominal(nominal) => pattern_uses_macro_call_metadata(&nominal.argument),
        Pattern::StringLiteral(_) | Pattern::Splice(_) => false,
    }
}

fn type_contains_named(ty: &Type, expected: &str) -> bool {
    match ty {
        Type::Forall(ty) => forall_types(ty).any(|ty| type_contains_named(ty, expected)),
        Type::Named(named) => named.namespace.is_none() && named.name == expected,
        Type::Function(function) => {
            type_contains_named(&function.parameter, expected)
                || function
                    .effects
                    .resources
                    .iter()
                    .any(|resource| type_contains_named(&resource.value_type, expected))
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
        Type::EffectApplication(application) => {
            type_contains_named(&application.callee, expected)
                || application
                    .effects
                    .resources
                    .iter()
                    .any(|resource| type_contains_named(&resource.value_type, expected))
        }
        Type::Array(array) => {
            type_contains_named(&array.element, expected)
                || type_contains_named(&array.count, expected)
        }
        Type::Inferred(_) | Type::NumberLiteral(_) | Type::StringLiteral(_) | Type::Splice(_) => {
            false
        }
    }
}

fn expression_parameter_contains_syntax(expression: &Expression) -> bool {
    if let Expression::TypeAscription(ascription) = expression {
        return expression_parameter_contains_syntax(&ascription.value);
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
            Expression::TypeApplication(application) => {
                obviously_not_syntax(&application.value, arity)
            }
            Expression::TypeAscription(ascription) => {
                obviously_not_syntax(&ascription.value, arity)
            }
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
        Expression::TypeApplication(application) => obviously_not_syntax(&application.value, 0),
        Expression::TypeAscription(ascription) => obviously_not_syntax(&ascription.value, 0),
        Expression::Match(match_) => match_
            .arms
            .iter()
            .any(|arm| obviously_not_syntax(&arm.body, 0)),
        Expression::Logical(logical) => {
            obviously_not_syntax(&logical.left, 0) || obviously_not_syntax(&logical.right, 0)
        }
        Expression::Binary(binary) => {
            obviously_not_syntax(&binary.left, 0) || obviously_not_syntax(&binary.right, 0)
        }
        // Desugars to a method call, which may produce syntax.
        Expression::Pipe(_) => false,
        Expression::Unary(unary) => obviously_not_syntax(&unary.operand, 0),
        Expression::Try(try_) => obviously_not_syntax(&try_.operand, 0),
        // Desugars to a `Convert.convert` call, which may produce syntax.
        Expression::Conversion(_) => false,
        Expression::StringTemplate(_) => true,
        Expression::Loop(_) => true,
        Expression::Coro(_) | Expression::Await(_) => true,
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
        | Expression::RepeatedProduct(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Char(_)
        | Expression::Integer(_)
        | Expression::Float(_) => true,
    }
}

/// True when a macro body's tail position — through curried parameters,
/// type ascriptions, `match` arms, and a block's trailing expression — is a bare
/// `quote { ... }`. `quote` always produces opaque `Syntax`, so it can never
/// satisfy a concrete syntax result type such as `Expr`.
fn quote_at_tail(expression: &Expression, arity: usize) -> bool {
    if arity > 0 {
        return match expression {
            Expression::Function(function) => quote_at_tail(&function.body, arity - 1),
            Expression::TypeApplication(application) => quote_at_tail(&application.value, arity),
            Expression::TypeAscription(ascription) => quote_at_tail(&ascription.value, arity),
            _ => false,
        };
    }
    match expression {
        Expression::Quote(quote) => quote.kind == staple_syntax::QuoteKind::Quote,
        Expression::TypeApplication(application) => quote_at_tail(&application.value, 0),
        Expression::TypeAscription(ascription) => quote_at_tail(&ascription.value, 0),
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
            Expression::TypeApplication(application) => {
                directly_evaluates_resource(&application.value, arity)
            }
            Expression::TypeAscription(ascription) => {
                directly_evaluates_resource(&ascription.value, arity)
            }
            _ => false,
        };
    }
    match expression {
        Expression::Resource(_) | Expression::With(_) => true,
        Expression::TypeApplication(application) => {
            directly_evaluates_resource(&application.value, 0)
        }
        Expression::TypeAscription(ascription) => directly_evaluates_resource(&ascription.value, 0),
        _ => false,
    }
}

fn valid_macro_annotation(annotation: &Type) -> bool {
    macro_signature(annotation).is_some()
}

fn invalid_raw_syntax_shape(meta: &MetaType) -> bool {
    match meta {
        MetaType::Delimited(_, DelimitedMetaContents::Slotted(elements))
            if elements.as_slice() == [MetaType::Syntax] =>
        {
            false
        }
        MetaType::Delimited(
            _,
            DelimitedMetaContents::Slotted(elements) | DelimitedMetaContents::Product(elements),
        )
        | MetaType::SlottedProduct(elements)
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
        | MetaType::Visibility
        | MetaType::TypeParameter
        | MetaType::TypeParameters => true,
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

fn is_plain_identifier(name: &staple_syntax::NameExpression) -> bool {
    let mut tokens = name
        .syntax
        .tokens()
        .iter()
        .filter(|token| !token.kind.is_trivia());
    if tokens.clone().next().is_none() {
        return name.syntax.definition_module().is_some();
    }
    matches!(tokens.next(), Some(token) if token.kind == staple_syntax::TokenKind::Identifier)
        && tokens.next().is_none()
}

/// A parenthesized single element stands for the element itself.
fn ungroup_value(value: Value) -> Value {
    match value {
        Value::Product(mut fields, _) if fields.len() == 1 && fields[0].0.is_none() => {
            fields.remove(0).1
        }
        other => other,
    }
}

/// Whether applying `callee` updates its argument in place.
fn callee_takes_mut(callee: &Value) -> bool {
    match callee {
        Value::Function { function, .. } => mutable_parameter_name(&function.pattern).is_some(),
        Value::Helper(_, binding) => matches!(&binding.value,
            Some(Expression::Function(function)) if mutable_parameter_name(&function.pattern).is_some()),
        Value::Intrinsic(intrinsic) => intrinsic.takes_mut(),
        Value::TypeApplied(value, _) => callee_takes_mut(value),
        _ => false,
    }
}

/// The name a function's `mut` parameter binds, when its pattern is one.
fn mutable_parameter_name(pattern: &Pattern) -> Option<&str> {
    match pattern {
        Pattern::Binding(binding) if binding.mutable => Some(&binding.name),
        Pattern::At(at) if at.binding.mutable => Some(&at.binding.name),
        _ => None,
    }
}

fn bool_value(value: bool) -> Value {
    Value::Nominal(
        if value { "True" } else { "False" }.to_owned(),
        Box::new(Value::Product(Vec::new().into(), false)),
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
            staple_syntax::string_literal::decode(&pattern.literal)
                .is_ok_and(|literal| literal == value)
        }
        Pattern::Binding(binding) => {
            if binding.name == "UnstructuredModifier" {
                return matches!(&value, Value::Syntax(SyntaxValue::Modifier(modifier)) if modifier.invocation.doc.is_some());
            }
            if let Value::Syntax(SyntaxValue::Visibility(visibility)) = &value
                && matches!(
                    binding.name.as_str(),
                    "Private" | "Package" | "Public" | "PublicReprPackage" | "PublicRepr"
                )
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
                    && matches!(argument.as_ref(), Value::Product(values, _) if values.is_empty());
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
                Value::Product(mut fields, _) if fields.len() == 1 && fields[0].0.is_none() => {
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
        // A parenthesized single pattern is grouping, as in the type checker's
        // `bind_pattern_types`, so `Some (WrapperBody _)` matches the payload.
        // A genuine one-element product, as in `ParseError (message)`, still
        // destructures below.
        Pattern::Product(product)
            if product.elements.len() == 1
                && !matches!(&value, Value::Product(values, _) if values.len() == 1)
                && !matches!(&value, Value::Sequence(values) if values.len() == 1) =>
        {
            bind_pattern(&product.elements[0], value, environment)
        }
        Pattern::Product(product) => {
            let values = match value {
                Value::Product(values, _) => values,
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
                    Value::Product(
                        vec![
                            (
                                Some("callee".to_owned()),
                                Value::Syntax(SyntaxValue::from_expression(*call.callee)),
                            ),
                            (
                                Some("argument".to_owned()),
                                Value::Syntax(SyntaxValue::from_expression(*call.argument)),
                            ),
                        ]
                        .into(),
                        false,
                    ),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && let Value::Syntax(SyntaxValue::Modifier(modifier)) = &value
            {
                if pattern.name == "ModifierCall" && modifier.invocation.doc.is_none() {
                    return bind_pattern(
                        &pattern.argument,
                        modifier_call_value(&modifier.invocation),
                        environment,
                    );
                }
                if pattern.name == "UnstructuredModifier" && modifier.invocation.doc.is_some() {
                    return bind_pattern(
                        &pattern.argument,
                        Value::Product(Vec::new().into(), false),
                        environment,
                    );
                }
            }
            if pattern.namespace.is_none()
                && pattern.name == "ModifiedItem"
                && let Value::Syntax(SyntaxValue::Item(item)) = &value
                && let Item::Modified(modified) = item.as_ref()
            {
                return bind_pattern(
                    &pattern.argument,
                    modified_item_value(modified),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && let Value::Syntax(SyntaxValue::Parameters { kind, fields, .. }) = &value
                && pattern.name == format_meta_type(kind)
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(fields.clone().into(), false),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && pattern.name == "CharExpr"
                && let Value::Syntax(SyntaxValue::Unstructured(Expression::Char(c))) = &value
            {
                let Ok(c) = staple_syntax::string_literal::decode_char(&c.literal) else {
                    return false;
                };
                return bind_pattern(&pattern.argument, Value::Char(c), environment);
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
                && !matches!(item.as_ref(), Item::Modified(_) | Item::TypeDeclaration(_))
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(Vec::new().into(), false),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && let Some(kind) = delimiter_kind(&pattern.name)
                && let Value::Syntax(SyntaxValue::Delimited(delimited)) = &value
                && delimited.kind == kind
            {
                let contents = match &delimited.contents {
                    DelimitedValueContents::Slotted(values)
                        if matches!(
                            values.as_slice(),
                            [(None, Value::Syntax(SyntaxValue::Raw(_)))]
                        ) =>
                    {
                        values[0].1.clone()
                    }
                    DelimitedValueContents::Slotted(values) => {
                        Value::Product(values.clone().into(), true)
                    }
                    DelimitedValueContents::Product(values) => {
                        Value::Product(values.clone().into(), false)
                    }
                    DelimitedValueContents::Sequence(values) => {
                        Value::Sequence(values.clone().into())
                    }
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
                // Testing the empty arm must not materialize the entire tail.
                if let Pattern::Product(product) = pattern.argument.as_ref()
                    && product.elements.is_empty()
                {
                    return values.is_empty();
                }
                if let Pattern::Product(product) = pattern.argument.as_ref()
                    && let [Pattern::Binding(first), Pattern::Binding(rest)] =
                        product.elements.as_slice()
                    && first.name == "first"
                    && rest.name == "rest"
                {
                    let Some((head, _)) = values.split_first() else {
                        return false;
                    };
                    return bind_pattern(
                        &pattern.argument,
                        Value::Product(
                            vec![
                                (Some("first".to_owned()), head.clone()),
                                (Some("rest".to_owned()), Value::Sequence(values.tail())),
                            ]
                            .into(),
                            false,
                        ),
                        environment,
                    );
                }
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(
                        values.into_iter().map(|value| (None, value)).collect(),
                        false,
                    ),
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
                    Value::Product(
                        vec![
                            (Some("separator".to_owned()), *separator),
                            (
                                Some("elements".to_owned()),
                                Value::Sequence(elements.into()),
                            ),
                            (Some("trailing".to_owned()), bool_value(trailing)),
                        ]
                        .into(),
                        false,
                    ),
                    environment,
                );
                return matched;
            }
            if pattern.namespace.is_none()
                && pattern.name == "Comma"
                && matches!(value, Value::Syntax(SyntaxValue::Comma(_)))
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(Vec::new().into(), false),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && pattern.name == "Equals"
                && matches!(value, Value::Syntax(SyntaxValue::Equals(_)))
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(Vec::new().into(), false),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && pattern.name == "FatArrow"
                && matches!(value, Value::Syntax(SyntaxValue::FatArrow(_)))
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(Vec::new().into(), false),
                    environment,
                );
            }
            if pattern.namespace.is_none()
                && let Value::Syntax(SyntaxValue::Visibility(visibility)) = &value
                && visibility_pattern_matches(&pattern.name, visibility.kind)
            {
                return bind_pattern(
                    &pattern.argument,
                    Value::Product(Vec::new().into(), false),
                    environment,
                );
            }
            let Value::Nominal(name, value) = value else {
                return false;
            };
            pattern.name == name && bind_pattern(&pattern.argument, *value, environment)
        }
        Pattern::Splice(_) => false,
    }
}

/// The structured compile-time value of one type declaration body.
fn type_body_value(body: &staple_syntax::TypeBody) -> Value {
    match body.kind {
        staple_syntax::TypeBodyKind::Alias => Value::Nominal(
            "AliasBody".to_owned(),
            Box::new(Value::Syntax(SyntaxValue::Type(
                body.underlying
                    .clone()
                    .expect("alias body has an underlying type"),
            ))),
        ),
        staple_syntax::TypeBodyKind::Wrapper | staple_syntax::TypeBodyKind::From => Value::Nominal(
            if body.kind == staple_syntax::TypeBodyKind::From {
                "FromBody".to_owned()
            } else {
                "WrapperBody".to_owned()
            },
            Box::new(Value::Product(
                vec![
                    (
                        None,
                        Value::Syntax(SyntaxValue::Visibility(body.representation.clone())),
                    ),
                    (
                        None,
                        Value::Syntax(SyntaxValue::Type(
                            body.underlying
                                .clone()
                                .expect("constructor body has an underlying type"),
                        )),
                    ),
                ]
                .into(),
                false,
            )),
        ),
        staple_syntax::TypeBodyKind::Opaque => Value::Nominal(
            "OpaqueBody".to_owned(),
            Box::new(Value::Product(Vec::new().into(), false)),
        ),
    }
}

fn type_declaration_item_value(declaration: &staple_syntax::TypeDeclaration) -> Value {
    let identifier = |name: &str| {
        let mut syntax = if name == declaration.name {
            declaration.name_syntax.clone()
        } else {
            declaration.syntax.clone()
        };
        if let Some(index) = syntax.tokens().iter().position(|token| {
            token.kind == staple_syntax::TokenKind::Identifier && token.text == name
        }) {
            let start = syntax.token_range.start + index;
            syntax.token_range = start..start + 1;
        }
        Value::Syntax(SyntaxValue::Ident(staple_syntax::NameExpression {
            syntax,
            name: name.to_owned(),
        }))
    };
    let parameters: Vec<_> = declaration
        .type_parameters
        .iter()
        .flat_map(staple_syntax::TypeParameterPattern::names)
        .map(identifier)
        .collect();
    let mut declared_type = Type::Named(staple_syntax::NamedType {
        syntax: declaration.name_syntax.clone(),
        namespace: None,
        name: declaration.name.clone(),
    });
    for parameter in &declaration.type_parameters {
        let argument = type_parameter_pattern_type(parameter);
        declared_type = Type::Application(staple_syntax::TypeApplication {
            syntax: declaration.syntax.clone(),
            callee: Box::new(declared_type),
            argument: Box::new(argument),
            bracketed: true,
        });
    }
    let body = match &declaration.body {
        Some(body) => Value::Nominal("Some".to_owned(), Box::new(type_body_value(body))),
        None => Value::Nominal(
            "None".to_owned(),
            Box::new(Value::Product(Vec::new().into(), false)),
        ),
    };
    let resource_dependencies = declaration
        .resource_dependencies
        .as_ref()
        .map(|dependencies| {
            dependencies
                .resources
                .iter()
                .map(|resource| {
                    Value::Nominal(
                        "ResourceDependency".to_owned(),
                        Box::new(Value::Product(
                            vec![
                                (
                                    Some("resource_type".to_owned()),
                                    Value::Syntax(SyntaxValue::Type(resource.value_type.clone())),
                                ),
                                (Some("mutable".to_owned()), bool_value(resource.mutable)),
                            ]
                            .into(),
                            false,
                        )),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Product(
        vec![
            (Some("name".to_owned()), identifier(&declaration.name)),
            (
                Some("declared_type".to_owned()),
                Value::Syntax(SyntaxValue::Type(declared_type)),
            ),
            (
                Some("type_parameters".to_owned()),
                Value::Sequence(parameters.into()),
            ),
            (Some("body".to_owned()), body),
            (
                Some("resource_dependencies".to_owned()),
                Value::Sequence(resource_dependencies.into()),
            ),
        ]
        .into(),
        false,
    )
}

fn modified_item_value(modified: &staple_syntax::ModifiedItem) -> Value {
    let modifiers: Vec<_> = modified
        .modifiers
        .iter()
        .enumerate()
        .map(|(index, invocation)| {
            Value::Syntax(SyntaxValue::Modifier(OpaqueModifier {
                invocation: invocation.clone(),
                enclosing: (index == 0).then(|| modified.syntax.clone()),
            }))
        })
        .collect();
    Value::Product(
        vec![
            (
                Some("modifiers".to_owned()),
                Value::Sequence(modifiers.into()),
            ),
            (
                Some("item".to_owned()),
                Value::Syntax(SyntaxValue::Item(modified.item.clone())),
            ),
        ]
        .into(),
        false,
    )
}

fn parameter_node(parameter: &staple_syntax::TypeParameter) -> Value {
    let name = parameter
        .pattern
        .names()
        .first()
        .copied()
        .unwrap_or("")
        .to_owned();
    let mut name_syntax = parameter.pattern.syntax().clone();
    if let Some(index) = name_syntax
        .tokens()
        .iter()
        .position(|token| token.text == name)
    {
        name_syntax.token_range =
            name_syntax.token_range.start + index..name_syntax.token_range.start + index + 1;
    }
    let identifier = Value::Syntax(SyntaxValue::Ident(staple_syntax::NameExpression {
        syntax: name_syntax.clone(),
        name: name.clone(),
    }));
    let argument = Type::Named(staple_syntax::NamedType {
        syntax: name_syntax,
        namespace: None,
        name,
    });
    let default = match &parameter.default {
        Some(ty) => Value::Nominal(
            "Some".into(),
            Box::new(Value::Syntax(SyntaxValue::Type(ty.clone()))),
        ),
        None => Value::Nominal(
            "None".into(),
            Box::new(Value::Product(vec![].into(), false)),
        ),
    };
    Value::Syntax(SyntaxValue::Parameters {
        kind: MetaType::TypeParameter,
        syntax: parameter.syntax.clone(),
        fields: vec![
            (Some("name".into()), identifier),
            (Some("default".into()), default),
            (
                Some("declaration".into()),
                Value::Syntax(SyntaxValue::Raw(parameter.syntax.clone())),
            ),
            (
                Some("argument".into()),
                Value::Syntax(SyntaxValue::Type(argument)),
            ),
        ],
    })
}

fn parse_parameter_syntax(
    expected: &MetaType,
    syntax: &Syntax,
    next: &mut usize,
) -> Result<(Value, usize), staple_syntax::ParseError> {
    if *expected == MetaType::TypeParameter {
        let (parameter, consumed) = staple_syntax::parse_type_parameter_prefix(syntax, next)?;
        return Ok((parameter_node(&parameter), consumed));
    }
    let (parameters, consumed) = staple_syntax::parse_type_parameters_prefix(syntax, next)?;
    let mut argument_tokens = Vec::new();
    let tokens = parameters.syntax.tokens();
    let punctuation = |kind, text: &str| {
        let mut token = tokens[0].clone();
        token.kind = kind;
        token.text = text.into();
        token
    };
    let effects = parameters
        .parameters
        .iter()
        .filter(|parameter| {
            matches!(
                parameter.pattern,
                staple_syntax::TypeParameterPattern::Effect(_)
            )
        })
        .collect::<Vec<_>>();
    if !effects.is_empty() {
        argument_tokens.push(punctuation(staple_syntax::TokenKind::LBrace, "{"));
        for (index, parameter) in effects.iter().enumerate() {
            if index > 0 {
                argument_tokens.push(punctuation(staple_syntax::TokenKind::Comma, ","));
            }
            argument_tokens.extend(
                parameter
                    .pattern
                    .syntax()
                    .tokens()
                    .iter()
                    .filter(|token| parameter.pattern.names().contains(&token.text.as_str()))
                    .cloned(),
            );
        }
        argument_tokens.push(punctuation(staple_syntax::TokenKind::RBrace, "}"));
    }
    let ordinary = parameters
        .parameters
        .iter()
        .filter(|parameter| {
            !matches!(
                parameter.pattern,
                staple_syntax::TypeParameterPattern::Effect(_)
            )
        })
        .collect::<Vec<_>>();
    if !ordinary.is_empty() {
        argument_tokens.push(punctuation(staple_syntax::TokenKind::Operator, "<"));
        for (index, parameter) in ordinary.iter().enumerate() {
            if index > 0 {
                argument_tokens.push(punctuation(staple_syntax::TokenKind::Comma, ","));
            }
            argument_tokens.extend(
                parameter
                    .pattern
                    .syntax()
                    .tokens()
                    .iter()
                    .filter(|token| !token.kind.is_trivia())
                    .cloned(),
            );
        }
        argument_tokens.push(punctuation(staple_syntax::TokenKind::Operator, ">"));
    }
    let mut arguments = parameters.syntax.clone();
    arguments.token_range = 0..argument_tokens.len();
    arguments.tokens = argument_tokens.into();
    let fields = vec![
        (
            Some("parameters".into()),
            Value::Sequence(parameters.parameters.iter().map(parameter_node).collect()),
        ),
        (
            Some("declaration".into()),
            Value::Syntax(SyntaxValue::Raw(parameters.syntax.clone())),
        ),
        (
            Some("arguments".into()),
            Value::Syntax(SyntaxValue::Raw(arguments)),
        ),
        (
            Some("bounds".into()),
            Value::Syntax(SyntaxValue::Raw(parameters.bounds)),
        ),
    ];
    Ok((
        Value::Syntax(SyntaxValue::Parameters {
            kind: MetaType::TypeParameters,
            syntax: parameters.syntax,
            fields,
        }),
        consumed,
    ))
}

fn type_parameter_pattern_type(parameter: &staple_syntax::TypeParameterPattern) -> Type {
    match parameter {
        staple_syntax::TypeParameterPattern::Binding(binding) => {
            Type::Named(staple_syntax::NamedType {
                syntax: binding.syntax.clone(),
                namespace: None,
                name: binding.name.clone(),
            })
        }
        staple_syntax::TypeParameterPattern::Effect(binding) => {
            Type::Named(staple_syntax::NamedType {
                syntax: binding.syntax.clone(),
                namespace: None,
                name: binding.name.clone(),
            })
        }
        staple_syntax::TypeParameterPattern::Product(product) => {
            Type::Product(staple_syntax::ProductType {
                slotted: false,
                syntax: product.syntax.clone(),
                elements: product
                    .elements
                    .iter()
                    .map(|element| staple_syntax::TypeElement {
                        syntax: element.syntax().clone(),
                        name: None,
                        ty: type_parameter_pattern_type(element),
                        default: None,
                        spread: false,
                        mutable: false,
                        moved: false,
                    })
                    .collect(),
                variadic: false,
            })
        }
        staple_syntax::TypeParameterPattern::Splice(splice) => Type::Splice(splice.clone()),
    }
}

fn visibility_pattern_matches(name: &str, kind: VisibilityKind) -> bool {
    matches!(
        (name, kind),
        ("Private", VisibilityKind::Private)
            | ("Package", VisibilityKind::Package)
            | ("Public", VisibilityKind::Public)
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
        (MetaType::CharExpr, Value::Syntax(SyntaxValue::Unstructured(Expression::Char(_)))) => true,
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
        (MetaType::Modifier, Value::Syntax(SyntaxValue::Modifier(_))) => true,
        (MetaType::ModifierCall, Value::Syntax(SyntaxValue::Modifier(value))) => {
            value.invocation.doc.is_none()
        }
        (MetaType::UnstructuredModifier, Value::Syntax(SyntaxValue::Modifier(value))) => {
            value.invocation.doc.is_some()
        }
        (MetaType::ModifiedItem, Value::Syntax(SyntaxValue::Item(item))) => {
            matches!(item.as_ref(), Item::Modified(_))
        }
        (MetaType::TypeDeclarationItem, Value::Syntax(SyntaxValue::Item(item))) => {
            matches!(item.as_ref(), Item::TypeDeclaration(_))
        }
        (
            MetaType::TypeParameter | MetaType::TypeParameters,
            Value::Syntax(SyntaxValue::Parameters { kind, .. }),
        ) => expected == kind,
        (MetaType::ResourceDependency, Value::Nominal(name, _)) => name == "ResourceDependency",
        (MetaType::TypeBody, Value::Nominal(name, _)) => {
            matches!(
                name.as_str(),
                "AliasBody" | "WrapperBody" | "FromBody" | "OpaqueBody"
            )
        }
        (MetaType::UnstructuredItem, Value::Syntax(SyntaxValue::Item(item))) => {
            !matches!(item.as_ref(), Item::Modified(_) | Item::TypeDeclaration(_))
        }
        (MetaType::Comma, Value::Syntax(SyntaxValue::Comma(_))) => true,
        (MetaType::Equals, Value::Syntax(SyntaxValue::Equals(_))) => true,
        (MetaType::FatArrow, Value::Syntax(SyntaxValue::FatArrow(_))) => true,
        (MetaType::SlottedProduct(expected), Value::Product(values, _))
        | (MetaType::Product(expected), Value::Product(values, _)) => {
            expected.len() == values.len()
                && expected
                    .iter()
                    .zip(values)
                    .all(|(expected, (_, value))| meta_type_matches_value(expected, value))
        }
        (
            MetaType::SlottedProduct(expected) | MetaType::Product(expected),
            Value::Sequence(values),
        ) => {
            expected.len() == values.len()
                && expected
                    .iter()
                    .zip(values)
                    .all(|(expected, value)| meta_type_matches_value(expected, value))
        }
        (MetaType::Optional(expected), Value::Nominal(name, value)) => match name.as_str() {
            "None" => matches!(value.as_ref(), Value::Product(values, _) if values.is_empty()),
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
                        DelimitedValueContents::Slotted(values),
                        DelimitedMetaContents::Slotted(expected),
                    )
                    | (
                        DelimitedValueContents::Product(values),
                        DelimitedMetaContents::Product(expected),
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
        (MetaType::Visibility, Value::Syntax(SyntaxValue::Visibility(_))) => true,
        (MetaType::MacroCallMetadata, Value::Product(fields, _)) => {
            let fields = fields.to_vec();
            let [
                (Some(modifiers), Value::Sequence(modifiers_value)),
                (Some(visibility), Value::Syntax(SyntaxValue::Visibility(_))),
            ] = fields.as_slice()
            else {
                return false;
            };
            modifiers == "modifiers"
                && visibility == "visibility"
                && modifiers_value
                    .iter()
                    .all(|value| meta_type_matches_value(&MetaType::Modifier, value))
        }
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
            if let Some(generics) = &mut function.generics {
                substitute_generics(generics, environment, diagnostics)?;
            }
            substitute_pattern(&mut function.pattern, environment, diagnostics)?;
            if let Some(effects) = &mut function.effect_lower_bound {
                for resource in &mut effects.resources {
                    substitute_type(&mut resource.value_type, environment, diagnostics)?;
                }
            }
            *function.body = substitute_splices(&function.body, environment, diagnostics)?
        }
        Expression::TypeApplication(application) => {
            *application.value = substitute_splices(&application.value, environment, diagnostics)?;
            for argument in &mut application.arguments {
                substitute_type(argument, environment, diagnostics)?;
            }
        }
        Expression::TypeAscription(ascription) => {
            *ascription.value = substitute_splices(&ascription.value, environment, diagnostics)?;
            substitute_type(&mut ascription.ty, environment, diagnostics)?;
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
        Expression::Coro(coro) => {
            for item in &mut coro.body.items {
                substitute_block_item(item, environment, diagnostics)?;
            }
        }
        Expression::Await(await_) => {
            *await_.operand = substitute_splices(&await_.operand, environment, diagnostics)?;
        }
        Expression::Try(try_) => {
            *try_.operand = substitute_splices(&try_.operand, environment, diagnostics)?;
        }
        Expression::Conversion(conversion) => {
            *conversion.value = substitute_splices(&conversion.value, environment, diagnostics)?;
            substitute_type(&mut conversion.ty, environment, diagnostics)?;
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
            let mut expanded = Vec::new();
            for mut element in std::mem::take(&mut product.elements) {
                if let Expression::Splice(splice) = &element.value
                    && splice.repeated
                    && let Some(Value::Sequence(values)) =
                        environment.get(&splice.name).map(EnvironmentBinding::get)
                {
                    for value in values {
                        let Value::Syntax(syntax) = value else {
                            diagnostics.push(Diagnostic::new(
                                splice.syntax.span.clone(),
                                "a repeated expression splice requires syntax values",
                            ));
                            return None;
                        };
                        let mut entry = element.clone();
                        entry.value = syntax.into_expression()?;
                        expanded.push(entry);
                    }
                } else {
                    element.value = substitute_splices(&element.value, environment, diagnostics)?;
                    expanded.push(element);
                }
            }
            product.elements = expanded;
        }
        Expression::RepeatedProduct(repeated) => {
            *repeated.value = substitute_splices(&repeated.value, environment, diagnostics)?;
            substitute_type(&mut repeated.count, environment, diagnostics)?;
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
        Expression::Unary(unary) => {
            *unary.operand = substitute_splices(&unary.operand, environment, diagnostics)?;
        }
        Expression::Binary(binary) => {
            *binary.left = substitute_splices(&binary.left, environment, diagnostics)?;
            *binary.right = substitute_splices(&binary.right, environment, diagnostics)?;
        }
        Expression::Pipe(pipe) => {
            *pipe.receiver = substitute_splices(&pipe.receiver, environment, diagnostics)?;
            *pipe.stage = substitute_splices(&pipe.stage, environment, diagnostics)?;
        }
        Expression::Logical(logical) => {
            *logical.left = substitute_splices(&logical.left, environment, diagnostics)?;
            *logical.right = substitute_splices(&logical.right, environment, diagnostics)?;
            substitute_type(&mut logical.bool_type, environment, diagnostics)?;
        }
        Expression::StringTemplate(template) => {
            for part in &mut template.parts {
                if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
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
        | Expression::Char(_)
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
            for bound in &mut declaration.equality_bounds {
                substitute_equality_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.default_bounds {
                substitute_default_bound(bound, environment, diagnostics)?;
            }
            if let Some(underlying) = declaration
                .body
                .as_mut()
                .and_then(|body| body.underlying.as_mut())
            {
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
        Item::Submodule(submodule) if submodule.name.is_empty() => true,
        Item::Submodule(_) => false,
        Item::UseDeclaration(_) | Item::MacroDeclaration(_) => false,
    }
}

fn modified_type_declaration_mut(item: &mut Item) -> Option<&mut staple_syntax::TypeDeclaration> {
    match item {
        Item::Modified(modified) => modified_type_declaration_mut(&mut modified.item),
        Item::TypeDeclaration(declaration) => Some(declaration),
        _ => None,
    }
}

fn modified_binding_mut(item: &mut Item) -> Option<&mut Binding> {
    match item {
        Item::Modified(modified) => modified_binding_mut(&mut modified.item),
        Item::VisibilitySplice(splice) => modified_binding_mut(&mut splice.item),
        Item::Binding(binding) => Some(binding),
        _ => None,
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
                && declaration.representation_visibility() == Visibility::Private
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
    if let Some(generics) = &mut binding.companion_generics {
        substitute_generics(generics, environment, diagnostics)?;
    }
    if let Some(target) = &mut binding.companion_target {
        substitute_type(target, environment, diagnostics)?;
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
    bound: &mut staple_syntax::TraitBound,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    for argument in &mut bound.arguments {
        substitute_type(argument, environment, diagnostics)?;
    }
    Some(())
}

fn substitute_subtype_bound(
    bound: &mut staple_syntax::SubtypeBound,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    substitute_type(&mut bound.supertype, environment, diagnostics)
}

fn substitute_equality_bound(
    bound: &mut staple_syntax::EqualityBound,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    substitute_type(&mut bound.left, environment, diagnostics)?;
    substitute_type(&mut bound.right, environment, diagnostics)
}

fn substitute_default_bound(
    bound: &mut staple_syntax::DefaultTypeBound,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    substitute_type(&mut bound.default, environment, diagnostics)
}

fn qualified_macro_access_path(
    access: &staple_syntax::AccessExpression,
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
                *ty = Type::Named(staple_syntax::NamedType {
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
                    DelimitedValueContents::Slotted(values)
                    | DelimitedValueContents::Product(values) => {
                        values.into_iter().map(|(_, value)| value).collect()
                    }
                    DelimitedValueContents::Sequence(values) => values.to_vec(),
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
                        Some(staple_syntax::TypeElement {
                            syntax: identifier.syntax.clone(),
                            name: None,
                            ty: Type::Named(staple_syntax::NamedType {
                                syntax: identifier.syntax,
                                namespace: None,
                                name: identifier.name,
                            }),
                            default: None,
                            spread: false,
                            mutable: false,
                            moved: false,
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
                *ty = Type::Product(staple_syntax::ProductType {
                    slotted: false,
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
        Type::Forall(ty) => {
            substitute_generics(&mut ty.generics, environment, diagnostics)?;
            substitute_type(&mut ty.body, environment, diagnostics)?;
        }

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
                substitute_type(&mut resource.value_type, environment, diagnostics)?;
            }
            substitute_type(&mut function.result, environment, diagnostics)?;
        }
        Type::Application(application) => {
            substitute_type(&mut application.callee, environment, diagnostics)?;
            substitute_type(&mut application.argument, environment, diagnostics)?;
        }
        Type::EffectApplication(application) => {
            substitute_type(&mut application.callee, environment, diagnostics)?;
            for resource in &mut application.effects.resources {
                substitute_type(&mut resource.value_type, environment, diagnostics)?;
            }
        }
        Type::Array(array) => {
            substitute_type(&mut array.element, environment, diagnostics)?;
            substitute_type(&mut array.count, environment, diagnostics)?;
        }
        Type::Named(named) => {
            if let Some(namespace) = &mut named.namespace {
                substitute_identifier(namespace, &mut named.syntax, environment, diagnostics)?;
            }
            substitute_identifier(&mut named.name, &mut named.syntax, environment, diagnostics)?;
        }
        Type::Inferred(_) | Type::NumberLiteral(_) | Type::StringLiteral(_) => {}
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
            if let Some(annotation) = &mut product.annotation {
                substitute_type(annotation, environment, diagnostics)?;
            }
        }
        Pattern::Nominal(nominal) => {
            if let Some(annotation) = &mut nominal.annotation {
                substitute_type(annotation, environment, diagnostics)?;
            }
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
            for modifier in &mut invocation.modifiers {
                if let Some(argument) = &mut modifier.argument
                    && let Some(expression) = &mut argument.expression
                {
                    *expression = substitute_splices(expression, environment, diagnostics)?;
                }
            }
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
            for bound in &mut declaration.subtype_bounds {
                substitute_subtype_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.equality_bounds {
                substitute_equality_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.default_bounds {
                substitute_default_bound(bound, environment, diagnostics)?;
            }
            for associated in &mut declaration.associated_types {
                for bound in &mut associated.trait_bounds {
                    substitute_trait_bound(bound, environment, diagnostics)?;
                }
                for bound in &mut associated.subtype_bounds {
                    substitute_subtype_bound(bound, environment, diagnostics)?;
                }
                for bound in &mut associated.equality_bounds {
                    substitute_equality_bound(bound, environment, diagnostics)?;
                }
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
            for bound in &mut implementation.equality_bounds {
                substitute_equality_bound(bound, environment, diagnostics)?;
            }
            for argument in &mut implementation.arguments {
                substitute_type(argument, environment, diagnostics)?;
            }
            for associated in &mut implementation.associated_types {
                substitute_type(&mut associated.value, environment, diagnostics)?;
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
            for bound in &mut declaration.equality_bounds {
                substitute_equality_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut declaration.default_bounds {
                substitute_default_bound(bound, environment, diagnostics)?;
            }
            if let Some(underlying) = declaration
                .body
                .as_mut()
                .and_then(|body| body.underlying.as_mut())
            {
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
            substitute_type_parameter_list(
                &mut submodule.type_parameters,
                environment,
                diagnostics,
            )?;
            for bound in &mut submodule.trait_bounds {
                substitute_trait_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut submodule.subtype_bounds {
                substitute_subtype_bound(bound, environment, diagnostics)?;
            }
            for bound in &mut submodule.equality_bounds {
                substitute_equality_bound(bound, environment, diagnostics)?;
            }
            if let Some(target) = &mut submodule.companion_target {
                substitute_type(target, environment, diagnostics)?;
            }
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
    let visibility = match kind {
        VisibilityKind::Private => Visibility::Private,
        VisibilityKind::Package => Visibility::Package,
        VisibilityKind::Public => Visibility::Public,
    };
    match item {
        Item::ExternBlock(block) => block.visibility = visibility,
        Item::Submodule(submodule) => submodule.visibility = visibility,
        Item::UseDeclaration(declaration) => declaration.visibility = visibility,
        Item::TypeDeclaration(declaration) => declaration.visibility = visibility,
        Item::TraitDeclaration(declaration) => declaration.visibility = visibility,
        Item::Binding(binding) => binding.visibility = visibility,
        _ => {
            diagnostics.push(Diagnostic::new(
                span,
                "visibility may only be spliced onto `let`, `def`, `type`, `extern`, or `trait` declarations",
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
                        "repeated item splice `${}` requires `Sequence<Item>`",
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
    parameters: &mut Vec<staple_syntax::TypeParameterPattern>,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    let mut substituted = Vec::new();
    for parameter in std::mem::take(parameters) {
        let staple_syntax::TypeParameterPattern::Splice(splice) = parameter else {
            substituted.push(parameter);
            continue;
        };
        let value = environment.get(&splice.name).map(EnvironmentBinding::get);
        let Some(Value::Sequence(values)) = value else {
            let actual = match value {
                None => "an unknown value",
                Some(Value::Product(_, _)) => "a product",
                Some(Value::Syntax(_)) => "syntax",
                Some(_) => "another compile-time value",
            };
            diagnostics.push(Diagnostic::new(
                splice.syntax.span,
                format!(
                    "type-parameter splice `${}...` requires `Sequence<Ident<String>>`, but contains {actual}",
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
            substituted.push(staple_syntax::TypeParameterPattern::Binding(
                staple_syntax::TypeParameterBinding {
                    syntax: identifier.syntax,
                    name: identifier.name,
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
                format!("identifier splice `${splice}` requires `Ident<String>`"),
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
            for modifier in &mut invocation.modifiers {
                if let Some(argument) = &mut modifier.argument
                    && let Some(expression) = &mut argument.expression
                {
                    alpha_rename_expression(expression, mark, &mut scopes);
                }
            }
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
            if let Some(effects) = &mut function.effect_lower_bound {
                for resource in &mut effects.resources {
                    if let Some(binding) = &mut resource.binding {
                        let renamed = hygienic_name(&binding.name, mark);
                        scope.insert(binding.name.clone(), renamed.clone());
                        binding.name = renamed;
                    }
                }
            }
            scopes.push(scope);
            alpha_rename_expression(&mut function.body, mark, scopes);
            scopes.pop();
        }
        Expression::TypeApplication(application) => {
            alpha_rename_expression(&mut application.value, mark, scopes)
        }
        Expression::TypeAscription(ascription) => {
            alpha_rename_expression(&mut ascription.value, mark, scopes)
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
        Expression::Coro(coro) => {
            alpha_rename_block(&mut coro.body, mark, scopes);
        }
        Expression::Await(await_) => {
            alpha_rename_expression(&mut await_.operand, mark, scopes);
        }
        Expression::Try(try_) => {
            alpha_rename_expression(&mut try_.operand, mark, scopes);
        }
        Expression::Conversion(conversion) => {
            alpha_rename_expression(&mut conversion.value, mark, scopes);
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
        Expression::RepeatedProduct(repeated) => {
            alpha_rename_expression(&mut repeated.value, mark, scopes);
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
        Expression::Unary(unary) => {
            alpha_rename_expression(&mut unary.operand, mark, scopes);
        }
        Expression::Binary(binary) => {
            alpha_rename_expression(&mut binary.left, mark, scopes);
            alpha_rename_expression(&mut binary.right, mark, scopes);
        }
        Expression::Pipe(pipe) => {
            alpha_rename_expression(&mut pipe.receiver, mark, scopes);
            for argument in pipe.stage_arguments_mut() {
                alpha_rename_expression(argument, mark, scopes);
            }
        }
        Expression::Logical(logical) => {
            alpha_rename_expression(&mut logical.left, mark, scopes);
            alpha_rename_expression(&mut logical.right, mark, scopes);
        }
        Expression::StringTemplate(template) => {
            for part in &mut template.parts {
                if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
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
        | Expression::Char(_)
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
        Expression::TypeApplication(value) => &mut value.syntax,
        Expression::TypeAscription(value) => &mut value.syntax,
        Expression::Match(value) => &mut value.syntax,
        Expression::Loop(value) => &mut value.syntax,
        Expression::Coro(value) => &mut value.syntax,
        Expression::Await(value) => &mut value.syntax,
        Expression::Try(value) => &mut value.syntax,
        Expression::Conversion(value) => &mut value.syntax,
        Expression::Resource(value) => &mut value.syntax,
        Expression::With(value) => &mut value.syntax,
        Expression::Block(value) => &mut value.syntax,
        Expression::Product(value) => &mut value.syntax,
        Expression::RepeatedProduct(value) => &mut value.syntax,
        Expression::Call(value) => &mut value.syntax,
        Expression::Access(value) => &mut value.syntax,
        Expression::Index(value) => &mut value.syntax,
        Expression::Unary(value) => &mut value.syntax,
        Expression::Binary(value) => &mut value.syntax,
        Expression::Pipe(value) => &mut value.syntax,
        Expression::Logical(value) => &mut value.syntax,
        Expression::SyntaxArgument(value) => &mut value.syntax,
        Expression::VisibilityArgument(value) => &mut value.syntax,
        Expression::Quote(value) => &mut value.syntax,
        Expression::Splice(value) => &mut value.syntax,
        Expression::Name(value) => &mut value.syntax,
        Expression::Char(value) => &mut value.syntax,
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
            if let Some(annotation) = &mut product.annotation {
                freshen_type(expander, annotation, module, mark);
            }
        }
        Pattern::Nominal(nominal) => {
            expander.freshen_syntax(&mut nominal.syntax, module, mark);
            if let Some(annotation) = &mut nominal.annotation {
                freshen_type(expander, annotation, module, mark);
            }
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
            if let Some(syntax) = &mut submodule.generics_syntax {
                expander.freshen_syntax(syntax, module, mark);
            }
            for parameter in &mut submodule.type_parameters {
                freshen_type_parameter(expander, parameter, module, mark);
            }
            for bound in &mut submodule.trait_bounds {
                freshen_trait_bound(expander, bound, module, mark);
            }
            for bound in &mut submodule.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            for bound in &mut submodule.equality_bounds {
                freshen_equality_bound(expander, bound, module, mark);
            }
            if let Some(target) = &mut submodule.companion_target {
                freshen_type(expander, target, module, mark);
            }
            for item in &mut submodule.module.items {
                freshen_item(expander, item, module, mark);
            }
        }
        Item::TypeDeclaration(declaration) => {
            expander.freshen_syntax(&mut declaration.syntax, module, mark);
            // `name_syntax` is a sibling node, not reachable from `syntax`, so it
            // needs freshening in its own right. Without this, every expansion of
            // a `quote`d `type $name` template reuses the template identifier
            // node's id, and anything keyed on it (trait-impl argument
            // resolution, hover, goto-definition) conflates the declarations.
            expander.freshen_syntax(&mut declaration.name_syntax, module, mark);
            for parameter in &mut declaration.type_parameters {
                freshen_type_parameter(expander, parameter, module, mark);
            }
            for bound in &mut declaration.trait_bounds {
                freshen_trait_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.equality_bounds {
                freshen_equality_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.default_bounds {
                freshen_default_bound(expander, bound, module, mark);
            }
            if let Some(underlying) = declaration
                .body
                .as_mut()
                .and_then(|body| body.underlying.as_mut())
            {
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
    if let Some(generics) = &mut binding.companion_generics {
        freshen_generics(expander, generics, module, mark);
    }
    if let Some(target) = &mut binding.companion_target {
        freshen_type(expander, target, module, mark);
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
    parameter: &mut staple_syntax::TypeParameterPattern,
    module: ModuleId,
    mark: u64,
) {
    match parameter {
        staple_syntax::TypeParameterPattern::Binding(binding) => {
            expander.freshen_syntax(&mut binding.syntax, module, mark)
        }
        staple_syntax::TypeParameterPattern::Effect(binding) => {
            expander.freshen_syntax(&mut binding.syntax, module, mark)
        }
        staple_syntax::TypeParameterPattern::Product(product) => {
            expander.freshen_syntax(&mut product.syntax, module, mark);
            for element in &mut product.elements {
                freshen_type_parameter(expander, element, module, mark);
            }
        }
        staple_syntax::TypeParameterPattern::Splice(splice) => {
            expander.freshen_syntax(&mut splice.syntax, module, mark);
        }
    }
}

fn freshen_trait_bound(
    expander: &mut MacroExpander,
    bound: &mut staple_syntax::TraitBound,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut bound.syntax, module, mark);
    expander.freshen_syntax(&mut bound.trait_name.syntax, module, mark);
    for argument in &mut bound.arguments {
        freshen_type(expander, argument, module, mark);
    }
}

fn freshen_equality_bound(
    expander: &mut MacroExpander,
    bound: &mut staple_syntax::EqualityBound,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut bound.syntax, module, mark);
    freshen_type(expander, &mut bound.left, module, mark);
    freshen_type(expander, &mut bound.right, module, mark);
}

fn freshen_subtype_bound(
    expander: &mut MacroExpander,
    bound: &mut staple_syntax::SubtypeBound,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut bound.syntax, module, mark);
    expander.freshen_syntax(&mut bound.parameter.syntax, module, mark);
    freshen_type(expander, &mut bound.supertype, module, mark);
}

fn freshen_default_bound(
    expander: &mut MacroExpander,
    bound: &mut staple_syntax::DefaultTypeBound,
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
            for modifier in &mut invocation.modifiers {
                expander.freshen_syntax(&mut modifier.syntax, module, mark);
                if let Some(argument) = &mut modifier.argument {
                    expander.freshen_syntax(&mut argument.syntax, module, mark);
                    if let Some(expression) = &mut argument.expression {
                        expander.freshen_expression(expression, module, mark);
                    }
                }
            }
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
            // `name_syntax` is a sibling node, not reachable from `syntax`, so it
            // needs freshening in its own right. Without this, every expansion of
            // a `quote`d `type $name` template reuses the template identifier
            // node's id, and anything keyed on it (trait-impl argument
            // resolution, hover, goto-definition) conflates the declarations.
            expander.freshen_syntax(&mut declaration.name_syntax, module, mark);
            for parameter in &mut declaration.type_parameters {
                freshen_type_parameter(expander, parameter, module, mark);
            }
            for bound in &mut declaration.trait_bounds {
                freshen_trait_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.equality_bounds {
                freshen_equality_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.default_bounds {
                freshen_default_bound(expander, bound, module, mark);
            }
            if let Some(underlying) = declaration
                .body
                .as_mut()
                .and_then(|body| body.underlying.as_mut())
            {
                freshen_type(expander, underlying, module, mark);
            }
        }
        Item::TraitDeclaration(declaration) => {
            expander.freshen_syntax(&mut declaration.syntax, module, mark);
            for parameter in &mut declaration.type_parameters {
                freshen_type_parameter(expander, parameter, module, mark);
            }
            for associated in &mut declaration.associated_types {
                expander.freshen_syntax(&mut associated.syntax, module, mark);
                for bound in &mut associated.trait_bounds {
                    freshen_trait_bound(expander, bound, module, mark);
                }
                for bound in &mut associated.subtype_bounds {
                    freshen_subtype_bound(expander, bound, module, mark);
                }
                for bound in &mut associated.equality_bounds {
                    freshen_equality_bound(expander, bound, module, mark);
                }
            }
            for prerequisite in &mut declaration.prerequisites {
                freshen_trait_bound(expander, prerequisite, module, mark);
            }
            for bound in &mut declaration.subtype_bounds {
                freshen_subtype_bound(expander, bound, module, mark);
            }
            for bound in &mut declaration.equality_bounds {
                freshen_equality_bound(expander, bound, module, mark);
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
            for bound in &mut implementation.equality_bounds {
                freshen_equality_bound(expander, bound, module, mark);
            }
            expander.freshen_syntax(&mut implementation.trait_name.syntax, module, mark);
            for argument in &mut implementation.arguments {
                freshen_type(expander, argument, module, mark);
            }
            for associated in &mut implementation.associated_types {
                expander.freshen_syntax(&mut associated.syntax, module, mark);
                freshen_type(expander, &mut associated.value, module, mark);
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
        Type::NumberLiteral(ty) => &mut ty.syntax,
        Type::StringLiteral(ty) => &mut ty.syntax,
        Type::Named(ty) => &mut ty.syntax,
        Type::Product(ty) => &mut ty.syntax,
        Type::Sum(ty) => &mut ty.syntax,
        Type::Function(ty) => &mut ty.syntax,
        Type::Forall(ty) => &mut ty.syntax,
        Type::Application(ty) => &mut ty.syntax,
        Type::EffectApplication(ty) => &mut ty.syntax,
        Type::Array(ty) => &mut ty.syntax,
        Type::Splice(ty) => &mut ty.syntax,
    };
    expander.freshen_syntax(syntax, module, mark);
    match ty {
        Type::Forall(ty) => {
            freshen_generics(expander, &mut ty.generics, module, mark);
            freshen_type(expander, &mut ty.body, module, mark);
        }

        Type::Product(product) => {
            for element in &mut product.elements {
                expander.freshen_syntax(&mut element.syntax, module, mark);
                freshen_type(expander, &mut element.ty, module, mark);
                if let Some(default) = &mut element.default {
                    expander.freshen_expression(default, module, mark);
                }
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
                freshen_type(expander, &mut resource.value_type, module, mark);
            }
            for marker in [&mut function.mutable_marker, &mut function.moved_marker]
                .into_iter()
                .flatten()
            {
                expander.freshen_syntax(marker, module, mark);
            }
            freshen_type(expander, &mut function.result, module, mark);
        }
        Type::Application(application) => {
            freshen_type(expander, &mut application.callee, module, mark);
            freshen_type(expander, &mut application.argument, module, mark);
        }
        Type::EffectApplication(application) => {
            freshen_type(expander, &mut application.callee, module, mark);
            expander.freshen_syntax(&mut application.effects.syntax, module, mark);
            for resource in &mut application.effects.resources {
                freshen_type(expander, &mut resource.value_type, module, mark);
            }
        }
        Type::Array(array) => {
            freshen_type(expander, &mut array.element, module, mark);
            freshen_type(expander, &mut array.count, module, mark);
        }
        Type::Inferred(_)
        | Type::NumberLiteral(_)
        | Type::StringLiteral(_)
        | Type::Named(_)
        | Type::Splice(_) => {}
    }
}

fn forall_types(ty: &staple_syntax::ForallType) -> impl Iterator<Item = &Type> {
    std::iter::once(ty.body.as_ref())
        .chain(
            ty.generics
                .trait_bounds
                .iter()
                .flat_map(|bound| bound.arguments.iter()),
        )
        .chain(
            ty.generics
                .subtype_bounds
                .iter()
                .map(|bound| &bound.supertype),
        )
        .chain(
            ty.generics
                .equality_bounds
                .iter()
                .flat_map(|bound| [&bound.left, &bound.right]),
        )
}

fn substitute_generics(
    generics: &mut staple_syntax::GenericParameters,
    environment: &Environment,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    substitute_type_parameter_list(&mut generics.parameters, environment, diagnostics)?;
    for bound in &mut generics.trait_bounds {
        substitute_trait_bound(bound, environment, diagnostics)?;
    }
    for bound in &mut generics.subtype_bounds {
        substitute_subtype_bound(bound, environment, diagnostics)?;
    }
    for bound in &mut generics.equality_bounds {
        substitute_equality_bound(bound, environment, diagnostics)?;
    }
    Some(())
}

fn freshen_generics(
    expander: &mut MacroExpander,
    generics: &mut staple_syntax::GenericParameters,
    module: ModuleId,
    mark: u64,
) {
    expander.freshen_syntax(&mut generics.syntax, module, mark);
    for parameter in &mut generics.parameters {
        freshen_type_parameter(expander, parameter, module, mark);
    }
    for bound in &mut generics.trait_bounds {
        freshen_trait_bound(expander, bound, module, mark);
    }
    for bound in &mut generics.subtype_bounds {
        freshen_subtype_bound(expander, bound, module, mark);
    }
    for bound in &mut generics.equality_bounds {
        freshen_equality_bound(expander, bound, module, mark);
    }
}

/// Compile-time type conversion must reject generic function types instead
/// of treating them as an unknown type and silently accepting any value.
fn reject_compile_forall(ty: &Type, diagnostics: &mut Vec<Diagnostic>) {
    match ty {
        Type::Forall(ty) => {
            diagnostics.push(Diagnostic::new(
                ty.syntax.span.clone(),
                "generic function types are not supported here yet",
            ));
            for ty in forall_types(ty) {
                reject_compile_forall(ty, diagnostics);
            }
        }
        Type::Function(ty) => {
            reject_compile_forall(&ty.parameter, diagnostics);
            reject_compile_forall(&ty.result, diagnostics);
            for resource in &ty.effects.resources {
                reject_compile_forall(&resource.value_type, diagnostics);
            }
        }
        Type::Product(ty) => {
            for element in &ty.elements {
                reject_compile_forall(&element.ty, diagnostics);
            }
        }
        Type::Sum(ty) => {
            for alternative in &ty.alternatives {
                reject_compile_forall(alternative, diagnostics);
            }
        }
        Type::Application(ty) => {
            reject_compile_forall(&ty.callee, diagnostics);
            reject_compile_forall(&ty.argument, diagnostics);
        }
        Type::EffectApplication(ty) => {
            reject_compile_forall(&ty.callee, diagnostics);
            for resource in &ty.effects.resources {
                reject_compile_forall(&resource.value_type, diagnostics);
            }
        }
        Type::Array(ty) => {
            reject_compile_forall(&ty.element, diagnostics);
            reject_compile_forall(&ty.count, diagnostics);
        }
        Type::Inferred(_)
        | Type::Named(_)
        | Type::NumberLiteral(_)
        | Type::StringLiteral(_)
        | Type::Splice(_) => {}
    }
}

fn modifier_call_field_types() -> Vec<CompileType> {
    vec![
        CompileType::Meta(MetaType::Sequence(Box::new(MetaType::Ident(None)))),
        CompileType::Meta(MetaType::Ident(None)),
        CompileType::Meta(MetaType::Optional(Box::new(MetaType::Syntax))),
    ]
}

fn modifier_call_value(invocation: &ModifierInvocation) -> Value {
    // Each path segment gets the syntax of its own token so that splicing a
    // name emits just that name, not the whole `@path(argument)` invocation.
    let tokens = invocation.syntax.tokens();
    let segments = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !token.kind.is_trivia())
        .skip(1)
        .take_while(|(_, token)| token.kind != staple_syntax::TokenKind::LParen)
        .filter(|(_, token)| token.kind != staple_syntax::TokenKind::Dot)
        .map(|(index, _)| syntax_slice(&invocation.syntax, index, index + 1))
        .collect::<Vec<_>>();
    let path = invocation
        .namespace
        .as_deref()
        .map(|path| path.split('.').collect::<Vec<_>>())
        .unwrap_or_default();
    let ident = |index: usize, name: &str| {
        let syntax = if segments.len() == path.len() + 1 {
            segments[index].clone()
        } else {
            invocation.syntax.clone()
        };
        Value::Syntax(SyntaxValue::Ident(staple_syntax::NameExpression {
            syntax,
            name: name.to_owned(),
        }))
    };
    let namespace = path
        .iter()
        .enumerate()
        .map(|(index, name)| ident(index, name))
        .collect::<Vec<_>>();
    let argument = match &invocation.argument {
        Some(argument) => {
            let tokens = argument.syntax.tokens();
            let first = tokens
                .iter()
                .position(|token| !token.kind.is_trivia())
                .unwrap();
            let last = tokens
                .iter()
                .rposition(|token| !token.kind.is_trivia())
                .unwrap();
            Value::Nominal(
                "Some".to_owned(),
                Box::new(Value::Syntax(SyntaxValue::Raw(syntax_slice(
                    &argument.syntax,
                    first + 1,
                    last,
                )))),
            )
        }
        None => Value::Nominal(
            "None".to_owned(),
            Box::new(Value::Product(Vec::new().into(), false)),
        ),
    };
    Value::Product(
        vec![
            (
                Some("namespace".to_owned()),
                Value::Sequence(namespace.into()),
            ),
            (Some("name".to_owned()), ident(path.len(), &invocation.name)),
            (Some("argument".to_owned()), argument),
        ]
        .into(),
        false,
    )
}

fn vocabulary_argument_meta(ty: &Type) -> Option<MetaType> {
    let expected = if let Some((element, separator)) = separated_meta_type(ty) {
        MetaType::Delimited(
            DelimiterKind::Parenthesized,
            DelimitedMetaContents::Separated {
                element: Box::new(element),
                separator: Box::new(separator),
            },
        )
    } else {
        meta_type(ty)?
    };
    valid_modifier_argument(&expected).then_some(expected)
}
