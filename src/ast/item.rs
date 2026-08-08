use super::{Expression, Syntax, Type};

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
