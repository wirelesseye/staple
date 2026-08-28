use super::{
    DefaultTypeBound, Expression, NamedType, SubtypeBound, Syntax, TraitBound, Type,
    TypeParameterPattern,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub syntax: Syntax,
    /// Syntax for this module's optional bare `mod` declaration.
    pub declaration_syntax: Option<Syntax>,
    pub docs: Vec<String>,
    pub visibility: Visibility,
    pub modifiers: Vec<ModifierInvocation>,
    pub items: Vec<Item>,
}

impl Module {
    pub fn text(&self) -> String {
        self.syntax.text()
    }
}

impl Item {
    /// The syntax node covering this item, whichever variant it is.
    pub fn syntax(&self) -> &Syntax {
        match self {
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
            Item::Binding(value) => &value.syntax,
            Item::PatternBinding(value) => &value.syntax,
            Item::Assignment(value) => &value.syntax,
            Item::Return(value) => &value.syntax,
            Item::Break(value) => &value.syntax,
            Item::Continue(value) => &value.syntax,
            Item::Expression(value) => value.syntax(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Item {
    Modified(ModifiedItem),
    VisibilityMacroInvocation(VisibilityMacroInvocation),
    VisibilitySplice(VisibilitySplice),
    RepeatedItemSplice(RepeatedItemSplice),
    UseDeclaration(UseDeclaration),
    Submodule(Submodule),
    ExternBlock(ExternBlock),
    TypeDeclaration(TypeDeclaration),
    MacroDeclaration(MacroDeclaration),
    TraitDeclaration(TraitDeclaration),
    TraitImplementation(TraitImplementation),
    Binding(Binding),
    PatternBinding(PatternBinding),
    Assignment(Assignment),
    Return(ReturnItem),
    Break(BreakItem),
    Continue(ContinueItem),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedItemSplice {
    pub syntax: Syntax,
    pub name: String,
}

/// A function-style macro invocation with compiler-owned call metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityMacroInvocation {
    pub syntax: Syntax,
    /// Prefix modifiers supplied to the macro call, in source order.
    pub modifiers: Vec<ModifierInvocation>,
    pub visibility: VisibilitySyntax,
    pub expression: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilitySplice {
    pub syntax: Syntax,
    pub name: String,
    pub item: Box<Item>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityKind {
    Private,
    Package,
    Public,
    PublicReprPackage,
    PublicRepr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilitySyntax {
    pub syntax: Syntax,
    pub kind: VisibilityKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifiedItem {
    pub syntax: Syntax,
    pub modifiers: Vec<ModifierInvocation>,
    pub item: Box<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifierInvocation {
    pub syntax: Syntax,
    pub namespace: Option<String>,
    pub name: String,
    pub argument: Option<ModifierArgument>,
    /// Decoded documentation text for a synthetic `///` invocation.
    pub doc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifierArgument {
    /// Includes the argument-delimiting parentheses.
    pub syntax: Syntax,
    /// Present when the contents are also a valid expression.
    pub expression: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submodule {
    pub syntax: Syntax,
    pub docs: Vec<String>,
    pub visibility: Visibility,
    pub name: String,
    pub module: Module,
    /// True for a type-keyed `companion` namespace rather than an ordinary
    /// inline `mod` declaration.
    pub companion: bool,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
    pub companion_target: Option<Type>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Package,
    Public,
}

impl Visibility {
    pub fn meets(self, required: Self) -> bool {
        let rank = |visibility| match visibility {
            Self::Private => 0,
            Self::Package => 1,
            Self::Public => 2,
        };
        rank(self) >= rank(required)
    }
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
    /// A bare dotted path whose final component can be either a module name or
    /// an item imported from its parent module. Program loading records both
    /// candidates; name resolution selects the only valid interpretation.
    Dotted,
    Namespace,
    Glob,
    Selected(Vec<String>),
    Renamed {
        item: String,
        alias: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub syntax: Syntax,
    pub target: Expression,
    pub value: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnItem {
    pub syntax: Syntax,
    pub value: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakItem {
    pub syntax: Syntax,
    pub value: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueItem {
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
    pub name_syntax: Syntax,
    pub docs: Vec<String>,
    pub recursive_constructor: bool,
    pub visibility: Visibility,
    pub representation_visibility: Visibility,
    pub kind: TypeDeclarationKind,
    pub name: String,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
    pub default_bounds: Vec<DefaultTypeBound>,
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
    pub docs: Vec<String>,
    pub visibility: Visibility,
    pub name: String,
    pub modifier: bool,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
    pub annotation: Option<Type>,
    pub value: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitDeclaration {
    pub syntax: Syntax,
    pub docs: Vec<String>,
    pub visibility: Visibility,
    pub name: String,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub functional_dependencies: Vec<FunctionalDependency>,
    pub prerequisites: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
    pub default_bounds: Vec<DefaultTypeBound>,
    pub members: Vec<TraitMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionalDependency {
    pub syntax: Syntax,
    pub determinants: Vec<NamedType>,
    pub dependent: NamedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitMember {
    pub syntax: Syntax,
    pub docs: Vec<String>,
    pub name: String,
    pub annotation: Type,
    pub default: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitImplementation {
    pub syntax: Syntax,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
    /// Whether this is a negative implementation (`impl !Trait T {}`),
    /// declaring that `T` explicitly does not implement `Trait`.
    pub negative: bool,
    pub trait_name: NamedType,
    pub arguments: Vec<Type>,
    pub members: Vec<ImplementationMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplementationMember {
    pub syntax: Syntax,
    pub docs: Vec<String>,
    pub name: String,
    pub value: Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Let,
    Def,
    Const,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub syntax: Syntax,
    pub docs: Vec<String>,
    pub visibility: Visibility,
    pub kind: BindingKind,
    pub mutable: bool,
    pub signal: bool,
    /// Whether this binding is a member of an `extern` block, written as a
    /// bare `name: Type` with no `let`/`def`/`const` keyword in source.
    pub external: bool,
    pub name: String,
    pub type_parameters: Vec<TypeParameterPattern>,
    pub trait_bounds: Vec<TraitBound>,
    pub subtype_bounds: Vec<SubtypeBound>,
    pub annotation: Option<Type>,
    pub value: Option<Expression>,
}

impl Binding {
    pub fn keyword(&self) -> &'static str {
        match self.kind {
            BindingKind::Def => "def",
            BindingKind::Let => "let",
            BindingKind::Const => "const",
        }
    }

    pub fn declaration_prefix(&self) -> String {
        if self.external {
            return "<extern>".to_owned();
        }
        let mut prefix = self.keyword().to_owned();
        if self.mutable {
            prefix.push_str(" mut");
        }
        if self.signal {
            prefix.push_str(" signal");
        }
        prefix
    }
}
