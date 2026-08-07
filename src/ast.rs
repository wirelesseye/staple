use std::fmt;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Let,
    Def,
    Extern,
    Type,
    Alias,
    Const,
    Underscore,
    Identifier,
    String,
    Integer,
    Whitespace,
    Newline,
    LineComment,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    Equals,
    Arrow,
    Ellipsis,
    Star,
    Plus,
    Minus,
    Slash,
    Unknown,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, Self::Whitespace | Self::Newline | Self::LineComment)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxToken {
    pub kind: TokenKind,
    pub text: String,
    pub span: Range<usize>,
}

/// The exact source covered by an AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Syntax {
    pub span: Range<usize>,
    pub tokens: Vec<SyntaxToken>,
}

impl Syntax {
    pub fn text(&self) -> String {
        self.tokens
            .iter()
            .map(|token| token.text.as_str())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub syntax: Syntax,
    pub items: Vec<Item>,
}

impl SourceFile {
    pub fn text(&self) -> String {
        self.syntax.text()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    ExternBlock(ExternBlock),
    TypeDeclaration(TypeDeclaration),
    Binding(Binding),
    Statement(ExpressionStatement),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionStatement {
    pub syntax: Syntax,
    pub expression: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternBlock {
    pub syntax: Syntax,
    /// The ABI string exactly as written, including its quotes.
    pub abi: String,
    pub bindings: Vec<Binding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeDeclarationKind {
    Alias,
    Distinct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDeclaration {
    pub syntax: Syntax,
    pub kind: TypeDeclarationKind,
    pub name: String,
    pub underlying: Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Let,
    Def,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub syntax: Syntax,
    pub kind: BindingKind,
    pub name: String,
    pub annotation: Option<Type>,
    pub value: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Inferred(Syntax),
    Named(NamedType),
    Pointer(PointerType),
    List(ListType),
    Function(FunctionType),
    Variadic(Syntax),
}

impl Type {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Inferred(syntax) | Self::Variadic(syntax) => syntax,
            Self::Named(ty) => &ty.syntax,
            Self::Pointer(ty) => &ty.syntax,
            Self::List(ty) => &ty.syntax,
            Self::Function(ty) => &ty.syntax,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedType {
    pub syntax: Syntax,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerType {
    pub syntax: Syntax,
    pub is_const: bool,
    pub pointee: Box<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListType {
    pub syntax: Syntax,
    pub elements: Vec<TypeElement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeElement {
    pub syntax: Syntax,
    pub name: Option<String>,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionType {
    pub syntax: Syntax,
    pub parameter: Box<Type>,
    pub result: Box<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    Function(FunctionExpression),
    Block(BlockExpression),
    List(ListExpression),
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
            Self::List(expression) => &expression.syntax,
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
    pub parameter: Parameter,
    pub body: Box<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parameter {
    Value(ValueParameter),
    List(ListParameter),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueParameter {
    pub syntax: Syntax,
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListParameter {
    pub syntax: Syntax,
    pub elements: Vec<ValueParameter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockExpression {
    pub syntax: Syntax,
    pub items: Vec<BlockItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockItem {
    Binding(Binding),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListExpression {
    pub syntax: Syntax,
    pub elements: Vec<ListElement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListElement {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}
