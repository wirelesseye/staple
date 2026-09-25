//! Canonical structural keys for Stage 3 specialization.
//!
//! Stage 3.1 defines the owned, typed identity of source-function instances
//! and of the constructor-adapter and structural-method artifacts Stage 4
//! materializes. Keys are built from semantic IDs and structural checked data
//! only: display names, source spans, contextual defaults, and expanded
//! nominal representations never participate in equality. The worklist that
//! consumes these keys belongs to Stage 3.3.

#![allow(dead_code)] // Stage 3.1 defines and tests keys before Stage 3.3 uses them.

use staple_syntax::Diagnostic;

use crate::{
    CheckedEffectSet, CheckedFunctionType, CheckedMutation, CheckedResource, CheckedStateEffect,
    CheckedType, Origin, TypeId, TypeParameterId,
};

/// One canonical structural type. Every vector preserves the checked semantic
/// order (field order, alternative order, resource order, argument order);
/// nothing is re-sorted or normalized here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalType {
    Never,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    ISize,
    USize,
    F32,
    F64,
    NumberLiteral(u64),
    String,
    StringLiteralSet(Vec<String>),
    Ref(Box<CanonicalType>),
    Slice(Box<CanonicalType>),
    Buffer(Box<CanonicalType>),
    Array {
        element: Box<CanonicalType>,
        count: Box<CanonicalType>,
    },
    CString,
    CChar,
    /// A template-level declared type parameter. Concrete keys reject it; the
    /// parameter's display name and `sized` metadata are not identity.
    Parameter(TypeParameterId),
    Nominal {
        kind: CanonicalNominalKind,
        id: TypeId,
        arguments: Vec<CanonicalType>,
    },
    CPointer {
        pointee: Box<CanonicalType>,
    },
    Product {
        elements: Vec<CanonicalProductElement>,
        variadic: bool,
    },
    Sum {
        alternatives: Vec<CanonicalType>,
    },
    Function(Box<CanonicalFunctionType>),
}

/// The declared nominal form. Kept distinct for the same `TypeId` because the
/// checker's own equality treats `TypeConstructor`, `Opaque`, and `Distinct`
/// as different types even when an alias or representation expansion would
/// display them alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalNominalKind {
    TypeConstructor,
    Opaque,
    Distinct,
}

/// One product element: the declared field name (which the checker keeps as
/// part of product identity) and its canonical value type. Contextual
/// construction defaults are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalProductElement {
    pub name: Option<String>,
    pub value_type: CanonicalType,
}

/// A canonical function shape. `CheckedFunctionType::default` is a source
/// expression and never participates in identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalFunctionType {
    pub parameter: Box<CanonicalType>,
    pub parameter_style: CanonicalParameterStyle,
    pub mutations: Vec<CanonicalMutation>,
    pub moves: Vec<CanonicalMutation>,
    pub effects: CanonicalEffectSet,
    pub result: Box<CanonicalType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalParameterStyle {
    Single,
    Juxtaposed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CanonicalMutation {
    Whole,
    Element(usize),
}

/// A canonical effect row. Template keys may retain a declared effect-variable
/// ID; concrete keys require Stage 3.2 to have substituted it away.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalEffectSet {
    pub variable: Option<TypeParameterId>,
    pub resources: Vec<CanonicalResource>,
    pub state: Option<CanonicalStateEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalResource {
    pub value_type: CanonicalType,
    pub mutable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalStateEffect {
    Read,
    Write,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversionMode {
    /// Template keys keep declared type/effect parameters as typed IDs.
    Template,
    /// Concrete keys reject any unresolved parameter, effect variable, and
    /// `Inferred`/`Error` placeholder.
    Concrete,
}

impl CanonicalType {
    /// Converts a checked type into a template-level key. Declared parameters
    /// stay by semantic ID so nested closure and enclosing-parameter
    /// substitutions can be added by Stage 3.2.
    pub(crate) fn template(value_type: &CheckedType, origin: &Origin) -> Result<Self, Diagnostic> {
        Self::convert(value_type, origin, ConversionMode::Template)
    }

    /// Converts a checked type into a concrete key, rejecting unresolved
    /// parameters and checker placeholders with a diagnostic at the
    /// requesting record's origin.
    pub(crate) fn concrete(value_type: &CheckedType, origin: &Origin) -> Result<Self, Diagnostic> {
        Self::convert(value_type, origin, ConversionMode::Concrete)
    }

    fn convert(
        value_type: &CheckedType,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
        let key = match value_type {
            CheckedType::Inferred => {
                return Err(origin_diagnostic(
                    origin,
                    "cannot form a specialization key from an inferred type",
                ));
            }
            CheckedType::Error => {
                return Err(origin_diagnostic(
                    origin,
                    "cannot form a specialization key from an error type",
                ));
            }
            CheckedType::Never => CanonicalType::Never,
            CheckedType::I8 => CanonicalType::I8,
            CheckedType::I16 => CanonicalType::I16,
            CheckedType::I32 => CanonicalType::I32,
            CheckedType::I64 => CanonicalType::I64,
            CheckedType::U8 => CanonicalType::U8,
            CheckedType::U16 => CanonicalType::U16,
            CheckedType::U32 => CanonicalType::U32,
            CheckedType::U64 => CanonicalType::U64,
            CheckedType::ISize => CanonicalType::ISize,
            CheckedType::USize => CanonicalType::USize,
            CheckedType::F32 => CanonicalType::F32,
            CheckedType::F64 => CanonicalType::F64,
            CheckedType::NumberLiteral(value) => CanonicalType::NumberLiteral(*value),
            CheckedType::String => CanonicalType::String,
            CheckedType::StringLiteralSet(values) => {
                CanonicalType::StringLiteralSet(values.clone())
            }
            CheckedType::Ref(payload) => {
                CanonicalType::Ref(Box::new(Self::convert(payload, origin, mode)?))
            }
            CheckedType::Slice(payload) => {
                CanonicalType::Slice(Box::new(Self::convert(payload, origin, mode)?))
            }
            CheckedType::Buffer(payload) => {
                CanonicalType::Buffer(Box::new(Self::convert(payload, origin, mode)?))
            }
            CheckedType::Array { element, count } => CanonicalType::Array {
                element: Box::new(Self::convert(element, origin, mode)?),
                count: Box::new(Self::convert(count, origin, mode)?),
            },
            CheckedType::CString => CanonicalType::CString,
            CheckedType::CChar => CanonicalType::CChar,
            CheckedType::Parameter { id, name, .. } => match mode {
                ConversionMode::Template => CanonicalType::Parameter(*id),
                ConversionMode::Concrete => {
                    return Err(origin_diagnostic(
                        origin,
                        format!("type parameter `{name}` is not resolved for a concrete key"),
                    ));
                }
            },
            CheckedType::TypeConstructor { id, arguments, .. } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::TypeConstructor,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin, mode)?,
            },
            CheckedType::Opaque { id, arguments, .. } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::Opaque,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin, mode)?,
            },
            CheckedType::CPointer { pointee } => CanonicalType::CPointer {
                pointee: Box::new(Self::convert(pointee, origin, mode)?),
            },
            CheckedType::Product(product) => CanonicalType::Product {
                elements: product
                    .elements
                    .iter()
                    .map(|element| {
                        Ok(CanonicalProductElement {
                            name: element.name.clone(),
                            value_type: Self::convert(&element.value_type, origin, mode)?,
                        })
                    })
                    .collect::<Result<Vec<_>, Diagnostic>>()?,
                variadic: product.variadic,
            },
            CheckedType::Sum(sum) => CanonicalType::Sum {
                alternatives: Self::convert_arguments(&sum.alternatives, origin, mode)?,
            },
            CheckedType::Function(function) => CanonicalType::Function(Box::new(
                CanonicalFunctionType::convert(function, origin, mode)?,
            )),
            CheckedType::Distinct {
                id,
                arguments,
                representation: _,
                ..
            } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::Distinct,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin, mode)?,
            },
        };
        Ok(key)
    }

    fn convert_arguments(
        arguments: &[CheckedType],
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Vec<CanonicalType>, Diagnostic> {
        arguments
            .iter()
            .map(|argument| Self::convert(argument, origin, mode))
            .collect()
    }
}

impl CanonicalFunctionType {
    pub(crate) fn template(
        function: &CheckedFunctionType,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(function, origin, ConversionMode::Template)
    }

    pub(crate) fn concrete(
        function: &CheckedFunctionType,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(function, origin, ConversionMode::Concrete)
    }

    fn convert(
        function: &CheckedFunctionType,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
        Ok(CanonicalFunctionType {
            parameter: Box::new(CanonicalType::convert(&function.parameter, origin, mode)?),
            parameter_style: match function.parameter_style {
                staple_syntax::FunctionParameterStyle::Single => CanonicalParameterStyle::Single,
                staple_syntax::FunctionParameterStyle::Juxtaposed => {
                    CanonicalParameterStyle::Juxtaposed
                }
            },
            mutations: function.mutations.iter().cloned().map(Into::into).collect(),
            moves: function.moves.iter().cloned().map(Into::into).collect(),
            effects: CanonicalEffectSet::convert(&function.effects, origin, mode)?,
            result: Box::new(CanonicalType::convert(&function.result, origin, mode)?),
        })
    }
}

impl CanonicalEffectSet {
    pub(crate) fn template(
        effects: &CheckedEffectSet,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(effects, origin, ConversionMode::Template)
    }

    pub(crate) fn concrete(
        effects: &CheckedEffectSet,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(effects, origin, ConversionMode::Concrete)
    }

    fn convert(
        effects: &CheckedEffectSet,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
        let variable = match (&effects.variable, mode) {
            (None, _) => None,
            (Some(variable), ConversionMode::Template) => Some(variable.id),
            (Some(variable), ConversionMode::Concrete) => {
                return Err(origin_diagnostic(
                    origin,
                    format!(
                        "effect variable `{}` is not resolved for a concrete key",
                        variable.name
                    ),
                ));
            }
        };
        Ok(CanonicalEffectSet {
            variable,
            resources: effects
                .resources
                .iter()
                .map(|resource| CanonicalResource::convert(resource, origin, mode))
                .collect::<Result<Vec<_>, Diagnostic>>()?,
            state: effects.state.map(Into::into),
        })
    }
}

impl CanonicalResource {
    fn convert(
        resource: &CheckedResource,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
        Ok(CanonicalResource {
            value_type: CanonicalType::convert(&resource.value_type, origin, mode)?,
            mutable: resource.mutable,
        })
    }
}

impl From<CheckedMutation> for CanonicalMutation {
    fn from(mutation: CheckedMutation) -> Self {
        match mutation {
            CheckedMutation::Whole => CanonicalMutation::Whole,
            CheckedMutation::Element(index) => CanonicalMutation::Element(index),
        }
    }
}

impl From<CheckedStateEffect> for CanonicalStateEffect {
    fn from(state: CheckedStateEffect) -> Self {
        match state {
            CheckedStateEffect::Read => CanonicalStateEffect::Read,
            CheckedStateEffect::Write => CanonicalStateEffect::Write,
            CheckedStateEffect::ReadWrite => CanonicalStateEffect::ReadWrite,
        }
    }
}

fn origin_diagnostic(origin: &Origin, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(origin.span.clone(), message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use staple_syntax::{
        Expression, FunctionParameterStyle, IntegerExpression, Span, Syntax, SyntaxId,
    };

    use crate::{
        CheckedFunctionParameterDefault, CheckedProductType, CheckedSumType, CheckedTypeElement,
    };

    fn requesting_origin() -> Origin {
        Origin {
            syntax: SyntaxId(99),
            span: Span::from(10..20),
        }
    }

    fn nominal(id: usize, name: &str) -> CheckedType {
        CheckedType::TypeConstructor {
            id: TypeId(id),
            name: name.to_owned(),
            arguments: Vec::new(),
        }
    }

    fn opaque(id: usize, name: &str) -> CheckedType {
        CheckedType::Opaque {
            id: TypeId(id),
            name: name.to_owned(),
            arguments: Vec::new(),
        }
    }

    fn distinct(
        id: usize,
        name: &str,
        arguments: Vec<CheckedType>,
        representation: CheckedType,
    ) -> CheckedType {
        CheckedType::Distinct {
            id: TypeId(id),
            name: name.to_owned(),
            arguments,
            representation: Box::new(representation),
        }
    }

    fn element(
        name: Option<&str>,
        value_type: CheckedType,
        default: Option<Expression>,
    ) -> CheckedTypeElement {
        CheckedTypeElement {
            name: name.map(str::to_owned),
            value_type,
            default,
        }
    }

    fn integer_default(literal: &str) -> Expression {
        Expression::Integer(IntegerExpression {
            syntax: Syntax::compiler(),
            literal: literal.to_owned(),
        })
    }

    fn function_type(
        parameter: CheckedType,
        parameter_style: FunctionParameterStyle,
        mutations: Vec<CheckedMutation>,
        moves: Vec<CheckedMutation>,
        effects: CheckedEffectSet,
        result: CheckedType,
    ) -> CheckedFunctionType {
        CheckedFunctionType {
            parameter: Box::new(parameter),
            parameter_style,
            default: None,
            mutations,
            moves,
            effects,
            result: Box::new(result),
        }
    }

    fn canonical(value_type: &CheckedType) -> CanonicalType {
        CanonicalType::template(value_type, &requesting_origin())
            .expect("template conversion should succeed")
    }

    fn concrete(value_type: &CheckedType) -> CanonicalType {
        CanonicalType::concrete(value_type, &requesting_origin())
            .expect("concrete conversion should succeed")
    }

    fn canonical_function(function: &CheckedFunctionType) -> CanonicalFunctionType {
        CanonicalFunctionType::concrete(function, &requesting_origin())
            .expect("concrete function conversion should succeed")
    }

    fn node_count(value_type: &CanonicalType) -> usize {
        match value_type {
            CanonicalType::Ref(payload)
            | CanonicalType::Slice(payload)
            | CanonicalType::Buffer(payload)
            | CanonicalType::CPointer { pointee: payload } => 1 + node_count(payload),
            CanonicalType::Array { element, count } => 1 + node_count(element) + node_count(count),
            CanonicalType::Nominal { arguments, .. } => {
                1 + arguments.iter().map(node_count).sum::<usize>()
            }
            CanonicalType::Product { elements, .. } => {
                1 + elements
                    .iter()
                    .map(|element| node_count(&element.value_type))
                    .sum::<usize>()
            }
            CanonicalType::Sum { alternatives } => {
                1 + alternatives.iter().map(node_count).sum::<usize>()
            }
            CanonicalType::Function(function) => {
                1 + node_count(&function.parameter)
                    + node_count(&function.result)
                    + function
                        .effects
                        .resources
                        .iter()
                        .map(|resource| node_count(&resource.value_type))
                        .sum::<usize>()
            }
            _ => 1,
        }
    }

    #[test]
    fn template_conversion_covers_every_checked_type_variant() {
        let function = function_type(
            CheckedType::Product(CheckedProductType {
                elements: vec![element(Some("x"), CheckedType::I32, None)],
                variadic: false,
            }),
            FunctionParameterStyle::Juxtaposed,
            vec![CheckedMutation::Element(0)],
            vec![CheckedMutation::Whole],
            CheckedEffectSet {
                variable: Some(crate::CheckedEffectVariable {
                    id: TypeParameterId(4),
                    name: "E".to_owned(),
                }),
                resources: vec![CheckedResource {
                    value_type: CheckedType::I32,
                    mutable: true,
                }],
                state: Some(CheckedStateEffect::ReadWrite),
            },
            CheckedType::Never,
        );
        let variants = [
            CheckedType::Never,
            CheckedType::I8,
            CheckedType::I16,
            CheckedType::I32,
            CheckedType::I64,
            CheckedType::U8,
            CheckedType::U16,
            CheckedType::U32,
            CheckedType::U64,
            CheckedType::ISize,
            CheckedType::USize,
            CheckedType::F32,
            CheckedType::F64,
            CheckedType::NumberLiteral(9),
            CheckedType::String,
            CheckedType::StringLiteralSet(vec!["a".to_owned()]),
            CheckedType::Ref(Box::new(CheckedType::I32)),
            CheckedType::Slice(Box::new(CheckedType::I32)),
            CheckedType::Buffer(Box::new(CheckedType::I32)),
            CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(3)),
            },
            CheckedType::CString,
            CheckedType::CChar,
            CheckedType::Parameter {
                id: TypeParameterId(1),
                name: "T".to_owned(),
                sized: true,
            },
            nominal(2, "Named"),
            opaque(3, "Opaque"),
            CheckedType::CPointer {
                pointee: Box::new(CheckedType::I32),
            },
            CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: true,
            }),
            CheckedType::Sum(CheckedSumType {
                alternatives: vec![CheckedType::I32, CheckedType::F64],
            }),
            CheckedType::Function(function),
            distinct(4, "Distinct", Vec::new(), CheckedType::I32),
        ];
        for value_type in &variants {
            CanonicalType::template(value_type, &requesting_origin()).unwrap_or_else(
                |diagnostic| panic!("{value_type} failed to convert: {diagnostic}"),
            );
        }
        for placeholder in [CheckedType::Inferred, CheckedType::Error] {
            let diagnostic = CanonicalType::template(&placeholder, &requesting_origin())
                .expect_err("checker placeholders never form keys");
            assert_eq!(diagnostic.span, requesting_origin().span);
        }
    }

    #[test]
    fn equivalent_checked_types_deduplicate() {
        assert_eq!(
            canonical(&nominal(7, "First")),
            canonical(&nominal(7, "Second"))
        );
        assert_eq!(
            canonical(&opaque(8, "First")),
            canonical(&opaque(8, "Second"))
        );
        assert_eq!(
            canonical(&distinct(
                9,
                "First",
                vec![CheckedType::I32],
                CheckedType::I32,
            )),
            canonical(&distinct(
                9,
                "Second",
                vec![CheckedType::I32],
                CheckedType::Product(CheckedProductType {
                    elements: vec![element(Some("payload"), CheckedType::I32, None)],
                    variadic: false,
                }),
            )),
            "nominal identity ignores display names and expanded representations"
        );
        assert_eq!(
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![element(Some("x"), CheckedType::I32, None)],
                variadic: false,
            })),
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![element(
                    Some("x"),
                    CheckedType::I32,
                    Some(integer_default("1")),
                )],
                variadic: false,
            })),
            "contextual element defaults never participate in a key"
        );
        let with_default = CheckedFunctionType {
            default: Some(Box::new(CheckedFunctionParameterDefault {
                name: "x".to_owned(),
                value: integer_default("1"),
            })),
            ..function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )
        };
        assert_eq!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&with_default),
            "a function-source default never participates in a key"
        );
    }

    #[test]
    fn nominal_identity_counts_and_shapes_separate() {
        assert_ne!(
            canonical(&nominal(1, "Same")),
            canonical(&nominal(2, "Same"))
        );
        assert_ne!(
            canonical(&nominal(1, "Same")),
            canonical(&opaque(1, "Same")),
            "nominal kind is part of identity"
        );
        assert_ne!(
            canonical(&CheckedType::TypeConstructor {
                id: TypeId(1),
                name: "Same".to_owned(),
                arguments: vec![CheckedType::I32],
            }),
            canonical(&CheckedType::TypeConstructor {
                id: TypeId(1),
                name: "Same".to_owned(),
                arguments: vec![CheckedType::I64],
            }),
            "nominal arguments are part of identity"
        );
        let named = |name: Option<&str>| {
            CheckedType::Product(CheckedProductType {
                elements: vec![element(name, CheckedType::I32, None)],
                variadic: false,
            })
        };
        assert_ne!(canonical(&named(Some("x"))), canonical(&named(Some("y"))));
        assert_ne!(canonical(&named(Some("x"))), canonical(&named(None)));
        assert_ne!(
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![
                    element(None, CheckedType::I32, None),
                    element(None, CheckedType::I64, None),
                ],
                variadic: false,
            })),
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![
                    element(None, CheckedType::I64, None),
                    element(None, CheckedType::I32, None),
                ],
                variadic: false,
            })),
            "field order is identity"
        );
        assert_ne!(
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: false,
            })),
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: true,
            })),
            "the variadic flag is identity"
        );
        assert_ne!(
            canonical(&CheckedType::Sum(CheckedSumType {
                alternatives: vec![CheckedType::I32, CheckedType::I64],
            })),
            canonical(&CheckedType::Sum(CheckedSumType {
                alternatives: vec![CheckedType::I64, CheckedType::I32],
            })),
            "alternative order is identity"
        );
        assert!(matches!(
            concrete(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(2)),
            }),
            CanonicalType::Array { .. }
        ));
        assert_ne!(
            canonical(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(2)),
            }),
            canonical(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(3)),
            }),
            "repeated counts are identity"
        );
        assert_ne!(
            canonical(&CheckedType::NumberLiteral(1)),
            canonical(&CheckedType::NumberLiteral(2)),
            "literal payloads are identity"
        );
        assert_ne!(
            canonical(&CheckedType::StringLiteralSet(vec![
                "a".to_owned(),
                "b".to_owned()
            ])),
            canonical(&CheckedType::StringLiteralSet(vec![
                "b".to_owned(),
                "a".to_owned()
            ])),
            "literal-set order is preserved"
        );
    }

    #[test]
    fn function_details_and_effect_rows_separate() {
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Juxtaposed,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            "parameter style is identity"
        );
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                vec![CheckedMutation::Whole],
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            "mutation masks are identity"
        );
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                vec![CheckedMutation::Whole, CheckedMutation::Element(1)],
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                vec![CheckedMutation::Element(1), CheckedMutation::Whole],
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            "ordered move masks are identity"
        );
        let effects = |resources: Vec<CheckedResource>, state| CheckedEffectSet {
            variable: None,
            resources,
            state,
        };
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![
                        CheckedResource {
                            value_type: CheckedType::I32,
                            mutable: false,
                        },
                        CheckedResource {
                            value_type: CheckedType::F64,
                            mutable: false,
                        },
                    ],
                    None,
                ),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![
                        CheckedResource {
                            value_type: CheckedType::F64,
                            mutable: false,
                        },
                        CheckedResource {
                            value_type: CheckedType::I32,
                            mutable: false,
                        },
                    ],
                    None,
                ),
                CheckedType::I32,
            )),
            "resource order is identity"
        );
        assert_ne!(
            canonical(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![CheckedResource {
                        value_type: CheckedType::I32,
                        mutable: true,
                    }],
                    None,
                ),
                CheckedType::I32,
            ))),
            canonical(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![CheckedResource {
                        value_type: CheckedType::I32,
                        mutable: false,
                    }],
                    None,
                ),
                CheckedType::I32,
            ))),
            "resource mutability is identity"
        );
        let stateful = |state| {
            canonical(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(Vec::new(), state),
                CheckedType::I32,
            )))
        };
        assert_ne!(stateful(None), stateful(Some(CheckedStateEffect::Read)));
        assert_ne!(
            stateful(Some(CheckedStateEffect::Read)),
            stateful(Some(CheckedStateEffect::Write))
        );
        assert_ne!(
            stateful(Some(CheckedStateEffect::Write)),
            stateful(Some(CheckedStateEffect::ReadWrite))
        );
        assert_ne!(
            stateful(Some(CheckedStateEffect::ReadWrite)),
            stateful(Some(CheckedStateEffect::Read))
        );
        assert_ne!(
            canonical(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(Vec::new(), None),
                CheckedType::I32,
            ))),
            canonical(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![CheckedResource {
                        value_type: CheckedType::I32,
                        mutable: false,
                    }],
                    None,
                ),
                CheckedType::I32,
            ))),
            "empty and nonempty effect rows are distinct"
        );
    }

    #[test]
    fn parameters_are_template_identity_only() {
        let first = CheckedType::Parameter {
            id: TypeParameterId(5),
            name: "First".to_owned(),
            sized: false,
        };
        let second = CheckedType::Parameter {
            id: TypeParameterId(5),
            name: "Second".to_owned(),
            sized: true,
        };
        assert_eq!(canonical(&first), canonical(&second));
        assert_eq!(
            canonical(&first),
            CanonicalType::Parameter(TypeParameterId(5))
        );

        let diagnostic = CanonicalType::concrete(&first, &requesting_origin())
            .expect_err("an unresolved parameter never forms a concrete key");
        assert_eq!(diagnostic.span, requesting_origin().span);
        assert!(diagnostic.message.contains("`First`"));
        assert!(
            CanonicalType::concrete(
                &CheckedType::Ref(Box::new(CheckedType::Parameter {
                    id: TypeParameterId(6),
                    name: "Nested".to_owned(),
                    sized: false,
                })),
                &requesting_origin(),
            )
            .is_err(),
            "unresolved parameters are rejected at any depth"
        );
    }

    #[test]
    fn unresolved_effects_and_placeholders_fail_concrete_keys() {
        let template = CheckedEffectSet {
            variable: Some(crate::CheckedEffectVariable {
                id: TypeParameterId(3),
                name: "Effects".to_owned(),
            }),
            resources: vec![CheckedResource {
                value_type: CheckedType::I32,
                mutable: false,
            }],
            state: Some(CheckedStateEffect::Read),
        };
        let key = CanonicalEffectSet::template(&template, &requesting_origin())
            .expect("template keys retain effect-variable IDs");
        assert_eq!(key.variable, Some(TypeParameterId(3)));

        let diagnostic = CanonicalEffectSet::concrete(&template, &requesting_origin())
            .expect_err("an unresolved effect variable never forms a concrete key");
        assert_eq!(diagnostic.span, requesting_origin().span);
        assert!(diagnostic.message.contains("`Effects`"));

        for placeholder in [CheckedType::Inferred, CheckedType::Error] {
            let diagnostic = CanonicalType::concrete(&placeholder, &requesting_origin())
                .expect_err("checker placeholders never form concrete keys");
            assert_eq!(diagnostic.span, requesting_origin().span);
        }
    }

    #[test]
    fn recursive_nominal_keys_stay_compact() {
        let mut expanded = distinct(11, "Node", vec![CheckedType::I32], CheckedType::I32);
        for _ in 0..64 {
            expanded = distinct(11, "Node", vec![CheckedType::I32], expanded);
        }
        let expanded_key = concrete(&expanded);
        let shallow_key = concrete(&distinct(
            11,
            "Node",
            vec![CheckedType::I32],
            CheckedType::I32,
        ));
        assert_eq!(expanded_key, shallow_key);
        assert_eq!(
            node_count(&expanded_key),
            node_count(&shallow_key),
            "the expanded representation never enters the key"
        );
    }
}
