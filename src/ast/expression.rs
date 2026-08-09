use super::{Pattern, Statement, Syntax, Type};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    Function(Box<FunctionExpression>),
    Satisfies(Box<SatisfiesExpression>),
    Match(MatchExpression),
    Block(BlockExpression),
    Product(ProductExpression),
    Call(CallExpression),
    Access(AccessExpression),
    Index(IndexExpression),
    Infix(InfixExpression),
    Quote(QuoteExpression),
    Splice(SpliceExpression),
    Name(NameExpression),
    String(StringExpression),
    CString(CStringExpression),
    Integer(IntegerExpression),
}

impl Expression {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Function(expression) => &expression.syntax,
            Self::Satisfies(expression) => &expression.syntax,
            Self::Match(expression) => &expression.syntax,
            Self::Block(expression) => &expression.syntax,
            Self::Product(expression) => &expression.syntax,
            Self::Call(expression) => &expression.syntax,
            Self::Access(expression) => &expression.syntax,
            Self::Index(expression) => &expression.syntax,
            Self::Infix(expression) => &expression.syntax,
            Self::Quote(expression) => &expression.syntax,
            Self::Splice(expression) => &expression.syntax,
            Self::Name(expression) => &expression.syntax,
            Self::String(expression) => &expression.syntax,
            Self::CString(expression) => &expression.syntax,
            Self::Integer(expression) => &expression.syntax,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteExpression {
    pub syntax: Syntax,
    pub template: Box<Expression>,
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
    pub statements: Vec<Statement>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductElement {
    pub syntax: Syntax,
    pub name: Option<String>,
    pub value: Expression,
    pub spread: bool,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accessor {
    Name(String),
    Index(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfixExpression {
    pub syntax: Syntax,
    pub operands: Vec<Expression>,
    pub operators: Vec<InfixOperator>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfixOperator {
    pub syntax: Syntax,
    pub namespace: Option<String>,
    pub name: String,
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
