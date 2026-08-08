use super::Syntax;

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
    pub namespace: Option<String>,
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
