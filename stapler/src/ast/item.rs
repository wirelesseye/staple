use super::{Expression, NamedType, Syntax, TraitBound, Type, TypeParameterPattern};

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
    UseDeclaration(UseDeclaration),
    ExternBlock(ExternBlock),
    TypeDeclaration(TypeDeclaration),
    MacroDeclaration(MacroDeclaration),
    TraitDeclaration(TraitDeclaration),
    TraitImplementation(TraitImplementation),
    Statement(Box<Statement>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Public,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseDeclaration {
    pub syntax: Syntax,
    pub visibility: Visibility,
    pub path: Vec<String>,
    pub kind: UseKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseKind {
    Namespace,
    Glob,
    Selected(Vec<String>),
    Renamed { item: String, alias: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Statement {
    Binding(Binding),
    PatternBinding(PatternBinding),
    Assignment(Assignment),
    Return(ReturnStatement),
    Break(BreakStatement),
    Continue(ContinueStatement),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub syntax: Syntax,
    pub target: Expression,
    pub value: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnStatement {
    pub syntax: Syntax,
    pub value: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakStatement {
    pub syntax: Syntax,
    pub value: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueStatement {
    pub syntax: Syntax,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternBlock {
    pub syntax: Syntax,
    pub visibility: Visibility,
    /// The ABI string exactly as written, including its quotes.
    pub abi: String,
    pub bindings: Vec<Binding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeDeclarationKind {
    Alias,
    Distinct,
    Singleton,
    Opaque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDeclaration {
    pub syntax: Syntax,
    pub visibility: Visibility,
    pub representation_visibility: Visibility,
    pub kind: TypeDeclarationKind,
    pub name: String,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub underlying: Option<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternBinding {
    pub syntax: Syntax,
    pub kind: PatternBindingKind,
    pub pattern: super::Pattern,
    pub value: Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternBindingKind {
    Irrefutable,
    Propagating,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDeclaration {
    pub syntax: Syntax,
    pub visibility: Visibility,
    pub name: String,
    pub annotation: Option<Type>,
    pub value: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitDeclaration {
    pub syntax: Syntax,
    pub visibility: Visibility,
    pub name: String,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub prerequisites: Vec<TraitBound>,
    pub members: Vec<TraitMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitMember {
    pub syntax: Syntax,
    pub name: String,
    pub annotation: Type,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitImplementation {
    pub syntax: Syntax,
    pub trait_name: NamedType,
    pub arguments: Vec<Type>,
    pub members: Vec<ImplementationMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplementationMember {
    pub syntax: Syntax,
    pub name: String,
    pub value: Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Let,
    Def,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Associativity {
    None,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fixity {
    pub associativity: Associativity,
    pub precedence: u8,
}

impl Default for Fixity {
    fn default() -> Self {
        Self {
            associativity: Associativity::Left,
            precedence: 9,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub syntax: Syntax,
    pub visibility: Visibility,
    pub kind: BindingKind,
    pub mutable: bool,
    pub name: String,
    pub fixity: Option<Fixity>,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub annotation: Option<Type>,
    pub value: Option<Expression>,
}
