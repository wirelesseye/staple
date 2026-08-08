use std::fmt;
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxId(pub usize);

impl SyntaxId {
    pub const COMPILER: Self = Self(usize::MAX);
}

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
    FatArrow,
    Ellipsis,
    Star,
    Plus,
    Minus,
    Slash,
    I32,
    Bool,
    Unknown,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, Self::Whitespace | Self::Newline | Self::LineComment)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    User(Range<usize>),
    Compiler,
}

impl Span {
    pub fn to_range(&self) -> Range<usize> {
        match self {
            Span::User(range) => range.clone(),
            Span::Compiler => unreachable!(),
        }
    }
}

impl From<Range<usize>> for Span {
    fn from(value: Range<usize>) -> Self {
        Self::User(value)
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
    pub id: SyntaxId,
    pub span: Span,
    pub(crate) tokens: Arc<[SyntaxToken]>,
    pub(crate) token_range: Range<usize>,
}

impl Syntax {
    pub fn text(&self) -> String {
        self.tokens()
            .iter()
            .map(|token| token.text.as_str())
            .collect()
    }

    pub fn tokens(&self) -> &[SyntaxToken] {
        &self.tokens[self.token_range.clone()]
    }

    pub fn compiler() -> Self {
        Self {
            id: SyntaxId::COMPILER,
            span: Span::Compiler,
            tokens: Arc::from([]),
            token_range: 0..0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub syntax: Syntax,
    pub items: Vec<Item>,
}

impl Module {
    pub fn text(&self) -> String {
        self.syntax.text()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    ExternBlock(ExternBlock),
    TypeDeclaration(TypeDeclaration),
    Statement(Box<Statement>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Binding(Binding),
    Expression(Expression),
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
    Inferred(InferredType),
    Named(NamedType),
    Pointer(PointerType),
    Product(ProductType),
    Function(FunctionType),
    Primitive(PrimitiveType),
}

impl Type {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Inferred(ty) => &ty.syntax,
            Self::Named(ty) => &ty.syntax,
            Self::Pointer(ty) => &ty.syntax,
            Self::Product(ty) => &ty.syntax,
            Self::Function(ty) => &ty.syntax,
            Self::Primitive(ty) => ty.syntax(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredType {
    pub syntax: Syntax,
}

impl InferredType {
    pub fn new() -> Self {
        Self {
            syntax: Syntax::compiler(),
        }
    }
}

impl Default for InferredType {
    fn default() -> Self {
        Self::new()
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
pub struct ProductType {
    pub syntax: Syntax,
    pub elements: Vec<TypeElement>,
    pub variadic: bool,
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
pub enum PrimitiveType {
    I32(Syntax),
    Bool(Syntax),
}

impl PrimitiveType {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::I32(syntax) => syntax,
            Self::Bool(syntax) => syntax,
        }
    }
}

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
    pub parameter: Parameter,
    pub return_type: Option<Type>,
    pub body: Box<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parameter {
    Value(ValueParameter),
    Product(ProductParameter),
}

impl Parameter {
    pub fn ty(&self) -> Type {
        match self {
            Parameter::Value(value_parameter) => value_parameter.ty.clone(),
            Parameter::Product(product_parameter) => {
                let elements = product_parameter
                    .elements
                    .iter()
                    .map(|param| param.type_element())
                    .collect();
                Type::Product(ProductType {
                    syntax: product_parameter.syntax.clone(),
                    elements,
                    variadic: false,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueParameter {
    pub syntax: Syntax,
    pub name: String,
    pub ty: Type,
}

impl ValueParameter {
    pub fn type_element(&self) -> TypeElement {
        TypeElement {
            syntax: self.syntax.clone(),
            name: Some(self.name.clone()),
            ty: self.ty.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductParameter {
    pub syntax: Syntax,
    pub elements: Vec<ValueParameter>,
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
