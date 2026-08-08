use super::{Pattern, Statement, Syntax, Type};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    Function(Box<FunctionExpression>),
    Block(BlockExpression),
    Product(ProductExpression),
    Call(CallExpression),
    Access(AccessExpression),
    Binary(BinaryExpression),
    Name(NameExpression),
    String(StringExpression),
    Integer(IntegerExpression),
}

impl Expression {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Function(expression) => &expression.syntax,
            Self::Block(expression) => &expression.syntax,
            Self::Product(expression) => &expression.syntax,
            Self::Call(expression) => &expression.syntax,
            Self::Access(expression) => &expression.syntax,
            Self::Binary(expression) => &expression.syntax,
            Self::Name(expression) => &expression.syntax,
            Self::String(expression) => &expression.syntax,
            Self::Integer(expression) => &expression.syntax,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionExpression {
    pub syntax: Syntax,
    pub pattern: Pattern,
    pub return_type: Option<Type>,
    pub body: Box<Expression>,
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
pub enum Accessor {
    Name(String),
    Index(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryExpression {
    pub syntax: Syntax,
    pub operator: BinaryOperator,
    pub left: Box<Expression>,
    pub right: Box<Expression>,
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
pub struct IntegerExpression {
    pub syntax: Syntax,
    /// The literal exactly as written.
    pub literal: String,
}
