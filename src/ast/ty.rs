use super::Syntax;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Inferred(InferredType),
    Named(NamedType),
    Product(ProductType),
    Function(FunctionType),
    Application(TypeApplication),
}

impl Type {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Inferred(ty) => &ty.syntax,
            Self::Named(ty) => &ty.syntax,
            Self::Product(ty) => &ty.syntax,
            Self::Function(ty) => &ty.syntax,
            Self::Application(ty) => &ty.syntax,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeParameterPattern {
    Binding(TypeParameterBinding),
    Product(TypeParameterProduct),
}

impl TypeParameterPattern {
    pub fn syntax(&self) -> &Syntax {
        match self {
            Self::Binding(binding) => &binding.syntax,
            Self::Product(product) => &product.syntax,
        }
    }

    pub fn names(&self) -> Vec<&str> {
        match self {
            Self::Binding(binding) => vec![binding.name.as_str()],
            Self::Product(product) => product
                .elements
                .iter()
                .flat_map(TypeParameterPattern::names)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameterBinding {
    pub syntax: Syntax,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameterProduct {
    pub syntax: Syntax,
    pub elements: Vec<TypeParameterPattern>,
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
    pub namespace: Option<String>,
    pub name: String,
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
pub struct TypeApplication {
    pub syntax: Syntax,
    pub callee: Box<Type>,
    pub argument: Box<Type>,
}
