use super::{Item, Pattern, Syntax, Type, VisibilitySyntax};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    Function(Box<FunctionExpression>),
    Satisfies(Box<SatisfiesExpression>),
    Match(MatchExpression),
    Loop(LoopExpression),
    Resource(Box<ResourceExpression>),
    With(Box<WithResourceExpression>),
    Block(BlockExpression),
    Product(ProductExpression),
    RepeatedProduct(RepeatedProductExpression),
    Call(CallExpression),
    Access(AccessExpression),
    Index(IndexExpression),
    Binary(BinaryExpression),
    Logical(LogicalExpression),
    SyntaxArgument(SyntaxArgumentExpression),
    VisibilityArgument(VisibilitySyntax),
    Quote(QuoteExpression),
    Splice(SpliceExpression),
    Name(NameExpression),
    String(StringExpression),
    StringTemplate(StringTemplateExpression),
    CString(CStringExpression),
    Integer(IntegerExpression),
    Float(FloatExpression),
}

impl Expression {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Function(expression) => &expression.syntax,
            Self::Satisfies(expression) => &expression.syntax,
            Self::Match(expression) => &expression.syntax,
            Self::Loop(expression) => &expression.syntax,
            Self::Resource(expression) => &expression.syntax,
            Self::With(expression) => &expression.syntax,
            Self::Block(expression) => &expression.syntax,
            Self::Product(expression) => &expression.syntax,
            Self::RepeatedProduct(expression) => &expression.syntax,
            Self::Call(expression) => &expression.syntax,
            Self::Access(expression) => &expression.syntax,
            Self::Index(expression) => &expression.syntax,
            Self::Binary(expression) => &expression.syntax,
            Self::Logical(expression) => &expression.syntax,
            Self::SyntaxArgument(expression) => &expression.syntax,
            Self::VisibilityArgument(expression) => &expression.syntax,
            Self::Quote(expression) => &expression.syntax,
            Self::Splice(expression) => &expression.syntax,
            Self::Name(expression) => &expression.syntax,
            Self::String(expression) => &expression.syntax,
            Self::StringTemplate(expression) => &expression.syntax,
            Self::CString(expression) => &expression.syntax,
            Self::Integer(expression) => &expression.syntax,
            Self::Float(expression) => &expression.syntax,
        }
    }
}

/// A parenthesized macro argument whose contents are not an expression.
///
/// Expansion reparses the inner tokens according to a macro parameter's
/// syntax category. This node must not survive macro expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxArgumentExpression {
    pub syntax: Syntax,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopExpression {
    pub syntax: Syntax,
    pub body: BlockExpression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceExpression {
    pub syntax: Syntax,
    pub resource: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithResourceExpression {
    pub syntax: Syntax,
    pub resource: Type,
    pub mutable: bool,
    pub value: Box<Expression>,
    pub body: BlockExpression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteExpression {
    pub syntax: Syntax,
    pub kind: QuoteKind,
    pub path: Vec<String>,
    pub contents: Syntax,
    pub template: QuoteTemplate,
}

/// Distinguishes the two compiler-provided quotation macros: `quote`, which
/// always produces an opaque `Syntax` fragment, and `parse_quote`, which
/// parses into whatever syntax category is expected at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    Quote,
    ParseQuote,
}

impl QuoteKind {
    pub fn name(self) -> &'static str {
        match self {
            QuoteKind::Quote => "quote",
            QuoteKind::ParseQuote => "parse_quote",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuoteTemplate {
    Expression(Box<Expression>),
    Item(Box<Item>),
    Items(Vec<Item>),
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpliceExpression {
    pub syntax: Syntax,
    pub name: String,
    pub repeated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchExpression {
    pub syntax: Syntax,
    pub subject: Box<Expression>,
    pub arms: Vec<MatchArm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchArm {
    pub syntax: Syntax,
    pub pattern: Pattern,
    pub body: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionExpression {
    pub syntax: Syntax,
    pub pattern: Pattern,
    pub body: Box<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SatisfiesExpression {
    pub syntax: Syntax,
    pub value: Box<Expression>,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockExpression {
    pub syntax: Syntax,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductExpression {
    pub syntax: Syntax,
    pub elements: Vec<ProductElement>,
}

impl ProductExpression {
    pub fn empty() -> Self {
        Self {
            syntax: Syntax::compiler(),
            elements: vec![],
        }
    }
}

/// A `(value; count)` product literal that repeats `value` `count` times.
///
/// `count` is an ordinary value expression that must fold to a non-negative
/// integer at compile time (the same evaluator used for `const` initializers).
/// `value` is evaluated exactly once; when the count is not `1` its type must
/// be `Copy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedProductExpression {
    pub syntax: Syntax,
    pub value: Box<Expression>,
    pub count: Box<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductElement {
    pub syntax: Syntax,
    pub name: Option<String>,
    /// Whether `name` was written as a contextual `.name:` designator rather
    /// than an ordinary positional `name:` label.
    pub designated: bool,
    pub value: Expression,
    pub spread: bool,
    /// Whether this is a `...=` spread, which merges the operand's named
    /// elements by name instead of by position.
    pub named_spread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallExpression {
    pub syntax: Syntax,
    pub callee: Box<Expression>,
    pub argument: Box<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessExpression {
    pub syntax: Syntax,
    pub value: Box<Expression>,
    pub accessor: Accessor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexExpression {
    pub syntax: Syntax,
    pub value: Box<Expression>,
    pub index: Box<Expression>,
}

/// A source-level infix operator expression. These nodes are retained through
/// macro expansion and lowered before name resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryExpression {
    pub syntax: Syntax,
    pub operator_syntax: Syntax,
    pub operator: BinaryOperator,
    pub left: Box<Expression>,
    pub right: Box<Expression>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Range,
    RangeInclusive,
    And,
    Or,
}

impl BinaryOperator {
    pub fn text(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Equal => "==",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessEqual => "<=",
            Self::Greater => ">",
            Self::GreaterEqual => ">=",
            Self::Range => "..",
            Self::RangeInclusive => "..=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}

/// `&&`/`||`. Not backed by a trait: the operands and result are always
/// `Bool`, and `right` is evaluated only when needed to determine the
/// result. `bool_type` is a compiler-synthesized reference to the prelude
/// `Bool` type, tied to the operator's own span, so name resolution and type
/// checking can resolve it exactly like a hand-written annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalExpression {
    pub syntax: Syntax,
    pub operator: LogicalOperator,
    pub left: Box<Expression>,
    pub right: Box<Expression>,
    pub bool_type: Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOperator {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accessor {
    Name(String),
    Index(String),
    Method(String),
    Representation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameExpression {
    pub syntax: Syntax,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringExpression {
    pub syntax: Syntax,
    /// The literal exactly as written, including its quotes.
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringTemplateExpression {
    pub syntax: Syntax,
    pub parts: Vec<StringTemplatePart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StringTemplatePart {
    Literal(String),
    Interpolation(StringInterpolation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringInterpolation {
    pub expression: Box<Expression>,
    pub format: StringInterpolationFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringInterpolationFormat {
    Display,
    Debug,
}

/// A C string literal emitted by the compiler-provided `c_string` macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CStringExpression {
    pub syntax: Syntax,
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerExpression {
    pub syntax: Syntax,
    /// The literal exactly as written.
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatExpression {
    pub syntax: Syntax,
    /// The literal exactly as written.
    pub literal: String,
}
