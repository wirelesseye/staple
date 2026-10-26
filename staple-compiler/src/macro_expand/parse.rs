//! Compile-time support for `std.syntax.Parse`.
//!
//! The built-in syntax nodes implement `Parse` with ordinary `impl` items in
//! `std/syntax.sta`; user programs add their own. The expander never hands
//! those items to the type checker. It collects them here as [`ParseImpl`]s,
//! picks one by matching the parse target against each impl's header, and runs
//! the impl's member with the macro interpreter. Leaf nodes bottom out in the
//! [`SyntaxIntrinsic`]s, which read tokens from a [`Value::Stream`].

use super::*;

/// The `std.syntax` functions the compiler implements itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SyntaxIntrinsic {
    Stream,
    DuplicateModifier,
    IsEmpty,
    Remaining,
    ErrorAt,
    ParsePrimitive,
    /// `__parenthesized_group`, `__bracketed_group`, `__braced_group`.
    Group(DelimiterKind),
    /// `__parenthesized`, `__bracketed`, `__braced`.
    Delimited(DelimiterKind),
    TakeRest,
    Offset,
    SequencePush,
    /// A float companion intrinsic (`__f64_floor`, `__f32_from_bits`, ...),
    /// so `const` initializers fold them with the results LLVM produces at
    /// run time.
    Float(crate::IntrinsicFunction),
    /// An integer companion intrinsic (`__i32_overflowing_add`,
    /// `__u64_count_ones`, ...), folded at the intrinsic's own width.
    Integer(crate::IntrinsicFunction),
}

impl SyntaxIntrinsic {
    pub(super) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "stream" => Self::Stream,
            "duplicate_modifier" => Self::DuplicateModifier,
            "is_empty" => Self::IsEmpty,
            "remaining" => Self::Remaining,
            "error_at" => Self::ErrorAt,
            "__parse_primitive" => Self::ParsePrimitive,
            "__parenthesized_group" => Self::Group(DelimiterKind::Parenthesized),
            "__bracketed_group" => Self::Group(DelimiterKind::Bracketed),
            "__angled_group" => Self::Group(DelimiterKind::Angled),
            "__braced_group" => Self::Group(DelimiterKind::Braced),
            "__parenthesized" => Self::Delimited(DelimiterKind::Parenthesized),
            "__bracketed" => Self::Delimited(DelimiterKind::Bracketed),
            "__angled" => Self::Delimited(DelimiterKind::Angled),
            "__braced" => Self::Delimited(DelimiterKind::Braced),
            "__take_rest" => Self::TakeRest,
            "__offset" => Self::Offset,
            "__sequence_push" => Self::SequencePush,
            _ => return None,
        })
    }

    /// The float intrinsic a name in a float companion declares.
    pub(super) fn from_float_name(name: &str) -> Option<Self> {
        let (float, suffix) = crate::FloatType::ALL.into_iter().find_map(|float| {
            name.strip_prefix(&format!("__{}_", float.intrinsic_name()))
                .map(|suffix| (float, suffix))
        })?;
        crate::float_companion_intrinsics(float)
            .into_iter()
            .find(|(candidate, _)| *candidate == suffix)
            .map(|(_, intrinsic)| Self::Float(intrinsic))
    }

    /// The integer intrinsic a name in an integer companion declares.
    pub(super) fn from_integer_name(name: &str) -> Option<Self> {
        let (integer, suffix) = crate::IntegerType::ALL.into_iter().find_map(|integer| {
            name.strip_prefix(&format!("__{}_", integer.intrinsic_name()))
                .map(|suffix| (integer, suffix))
        })?;
        crate::integer_companion_intrinsics(integer)
            .into_iter()
            .find(|(candidate, _)| *candidate == suffix)
            .map(|(_, intrinsic)| Self::Integer(intrinsic))
    }

    /// Whether the intrinsic's parameter is `mut`: it advances its argument in
    /// place.
    pub(super) fn takes_mut(self) -> bool {
        matches!(self, Self::ParsePrimitive | Self::Group(_) | Self::TakeRest)
    }

    /// Whether the intrinsic is generic over a call-site type argument.
    fn takes_type_argument(self) -> bool {
        matches!(self, Self::ParsePrimitive | Self::Delimited(_))
    }
}

/// What a type parameter of a `Parse` implementation stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TypeBinding {
    Meta(MetaType),
    /// The spelling `S` of an `Ident<S>`; `None` for `Ident<String>`.
    Spelling(Option<String>),
}

pub(super) type TypeEnv = Rc<HashMap<String, TypeBinding>>;

/// The `Parse` target a static check inferred for a call without a type
/// argument, from the type its result is expected to have.
pub(super) type ParseTarget = CompileType;

/// How an impl header's argument constrains a parse target. The same shape
/// also serves as a template for writing a target in terms of the impl's
/// parameters.
#[derive(Clone, Debug)]
pub(super) enum HeaderPattern {
    Exact(MetaType),
    Parameter(String),
    Ident(IdentPattern),
    Optional(Box<HeaderPattern>),
    Sequence(Box<HeaderPattern>),
    Delimited(DelimiterKind, Box<HeaderPattern>),
    Nominal(String, Vec<HeaderPattern>),
    /// `(A; B)` or `(A; B; ...T)`.
    Slotted(Vec<HeaderPattern>, Option<String>),
    /// `(A, B)` or `(A, B, ...T)`.
    Product(Vec<HeaderPattern>, Option<String>),
}

#[derive(Clone, Debug)]
pub(super) enum IdentPattern {
    Any,
    Spelling(String),
    Parameter(String),
}

pub(super) struct ParseImpl {
    pub module: ModuleId,
    pub header: HeaderPattern,
    /// The header as written, for diagnostics.
    pub header_text: String,
    pub members: HashMap<String, Expression>,
    /// The impl's type parameters, and the ones bounded by `Parse`.
    pub parameters: HashSet<String>,
    pub parse_bounds: HashSet<String>,
    /// The type `parse` produces when the header is concrete.
    pub target: CompileType,
}

pub(super) fn header_pattern(ty: &Type, parameters: &HashSet<String>) -> Option<HeaderPattern> {
    match ty {
        Type::Named(named) => {
            if named.namespace.is_none() && parameters.contains(&named.name) {
                return Some(HeaderPattern::Parameter(named.name.clone()));
            }
            if let Some(meta) = meta_type(ty) {
                return Some(HeaderPattern::Exact(meta));
            }
            named
                .name
                .chars()
                .next()
                .is_some_and(char::is_uppercase)
                .then(|| HeaderPattern::Nominal(named.name.clone(), Vec::new()))
        }
        Type::Application(_) => {
            let mut arguments = Vec::new();
            let mut head = ty;
            while let Type::Application(application) = head {
                arguments.push(application.argument.as_ref());
                head = application.callee.as_ref();
            }
            arguments.reverse();
            let Type::Named(name) = head else {
                return None;
            };
            if name.namespace.is_some() {
                return None;
            }
            let element = |ty: &Type| header_pattern(unwrap_singleton_product(ty), parameters);
            match (name.name.as_str(), arguments.as_slice()) {
                ("Option", [element_type]) => {
                    Some(HeaderPattern::Optional(Box::new(element(element_type)?)))
                }
                ("Sequence", [element_type]) => {
                    Some(HeaderPattern::Sequence(Box::new(element(element_type)?)))
                }
                ("Sequence", [Type::Named(ident), spelling])
                    if ident.namespace.is_none() && ident.name == "Ident" =>
                {
                    Some(HeaderPattern::Sequence(Box::new(HeaderPattern::Ident(
                        ident_pattern(spelling, parameters)?,
                    ))))
                }
                ("Ident", [spelling]) => {
                    Some(HeaderPattern::Ident(ident_pattern(spelling, parameters)?))
                }
                (delimiter, [contents]) if delimiter_kind(delimiter).is_some() => {
                    Some(HeaderPattern::Delimited(
                        delimiter_kind(delimiter)?,
                        Box::new(contents_pattern(contents, parameters)?),
                    ))
                }
                (name, arguments) => Some(HeaderPattern::Nominal(
                    name.to_owned(),
                    arguments
                        .iter()
                        .map(|argument| element(argument))
                        .collect::<Option<Vec<_>>>()?,
                )),
            }
        }
        Type::Product(product) => product_pattern(product, parameters),
        _ => None,
    }
}

fn ident_pattern(ty: &Type, parameters: &HashSet<String>) -> Option<IdentPattern> {
    match ty {
        Type::Named(named) if named.namespace.is_none() && named.name == "String" => {
            Some(IdentPattern::Any)
        }
        Type::Named(named) if named.namespace.is_none() && parameters.contains(&named.name) => {
            Some(IdentPattern::Parameter(named.name.clone()))
        }
        Type::StringLiteral(literal) => staple_syntax::string_literal::decode(&literal.literal)
            .ok()
            .map(IdentPattern::Spelling),
        _ => None,
    }
}

/// A delimiter's contents, read the way `delimiter_contents_meta` reads them.
fn contents_pattern(ty: &Type, parameters: &HashSet<String>) -> Option<HeaderPattern> {
    if let Type::Product(product) = ty
        && !product.variadic
        && product
            .elements
            .iter()
            .all(|element| element.name.is_none() && !element.spread)
    {
        let mut patterns = product
            .elements
            .iter()
            .map(|element| header_pattern(&element.ty, parameters))
            .collect::<Option<Vec<_>>>()?;
        return Some(if patterns.len() == 1 {
            patterns.remove(0)
        } else if product.slotted || patterns.len() < 2 {
            HeaderPattern::Slotted(patterns, None)
        } else {
            HeaderPattern::Product(patterns, None)
        });
    }
    header_pattern(ty, parameters)
}

fn product_pattern(
    product: &staple_syntax::ProductType,
    parameters: &HashSet<String>,
) -> Option<HeaderPattern> {
    if product.variadic
        || product
            .elements
            .iter()
            .any(|element| element.name.is_some())
    {
        return None;
    }
    let (fixed, tail) = match product.elements.split_last() {
        Some((last, fixed)) if last.spread => {
            let Type::Named(name) = &last.ty else {
                return None;
            };
            if !parameters.contains(&name.name) {
                return None;
            }
            (fixed, Some(name.name.clone()))
        }
        _ => (product.elements.as_slice(), None),
    };
    if fixed.iter().any(|element| element.spread) {
        return None;
    }
    let mut patterns = fixed
        .iter()
        .map(|element| header_pattern(&element.ty, parameters))
        .collect::<Option<Vec<_>>>()?;
    if tail.is_none() {
        if patterns.is_empty() {
            return Some(HeaderPattern::Exact(MetaType::Product(Vec::new())));
        }
        // A user-written `(T)` is `T`.
        if patterns.len() == 1 && !product.slotted {
            return patterns.pop();
        }
    }
    Some(if product.slotted {
        HeaderPattern::Slotted(patterns, tail)
    } else {
        HeaderPattern::Product(patterns, tail)
    })
}

/// A delimiter's contents as the target type of the nodes inside it.
pub(super) fn contents_meta(contents: &DelimitedMetaContents) -> MetaType {
    match contents {
        DelimitedMetaContents::Slotted(elements) if elements.len() == 1 => elements[0].clone(),
        DelimitedMetaContents::Slotted(elements) => MetaType::SlottedProduct(elements.clone()),
        DelimitedMetaContents::Product(elements) => MetaType::Product(elements.clone()),
        DelimitedMetaContents::Sequence(element) => MetaType::Sequence(element.clone()),
        DelimitedMetaContents::Separated { element, separator } => MetaType::Nominal(
            "Separated".to_owned(),
            vec![element.as_ref().clone(), separator.as_ref().clone()],
        ),
    }
}

/// The inverse of [`contents_meta`].
pub(super) fn meta_contents(meta: MetaType) -> DelimitedMetaContents {
    match meta {
        MetaType::SlottedProduct(elements) => DelimitedMetaContents::Slotted(elements),
        MetaType::Product(elements) => DelimitedMetaContents::Product(elements),
        MetaType::Sequence(element) => DelimitedMetaContents::Sequence(element),
        MetaType::Nominal(name, mut arguments) if name == "Separated" && arguments.len() == 2 => {
            let separator = arguments.remove(1);
            let element = arguments.remove(0);
            DelimitedMetaContents::Separated {
                element: Box::new(element),
                separator: Box::new(separator),
            }
        }
        other => DelimitedMetaContents::Slotted(vec![other]),
    }
}

fn is_unit(meta: &MetaType) -> bool {
    matches!(meta, MetaType::Product(elements) | MetaType::SlottedProduct(elements) if elements.is_empty())
}

fn same_meta(left: &MetaType, right: &MetaType) -> bool {
    (is_unit(left) && is_unit(right)) || left == right
}

fn bind_parameter(
    env: &mut HashMap<String, TypeBinding>,
    name: &str,
    binding: TypeBinding,
) -> bool {
    match env.get(name) {
        Some(existing) => *existing == binding,
        None => {
            env.insert(name.to_owned(), binding);
            true
        }
    }
}

/// Matches an impl header against a parse target, binding the impl's type
/// parameters on success.
pub(super) fn match_header(
    pattern: &HeaderPattern,
    target: &MetaType,
    env: &mut HashMap<String, TypeBinding>,
) -> bool {
    match pattern {
        HeaderPattern::Exact(expected) => same_meta(expected, target),
        HeaderPattern::Parameter(name) => {
            bind_parameter(env, name, TypeBinding::Meta(target.clone()))
        }
        HeaderPattern::Ident(ident) => {
            let MetaType::Ident(spelling) = target else {
                return false;
            };
            match ident {
                IdentPattern::Any => spelling.is_none(),
                IdentPattern::Spelling(expected) => spelling.as_deref() == Some(expected),
                IdentPattern::Parameter(name) => {
                    bind_parameter(env, name, TypeBinding::Spelling(spelling.clone()))
                }
            }
        }
        HeaderPattern::Optional(inner) => {
            matches!(target, MetaType::Optional(element) if match_header(inner, element, env))
        }
        HeaderPattern::Sequence(inner) => {
            matches!(target, MetaType::Sequence(element) if match_header(inner, element, env))
        }
        HeaderPattern::Delimited(kind, inner) => match target {
            MetaType::Delimited(target_kind, contents) if target_kind == kind => {
                match_header(inner, &contents_meta(contents), env)
            }
            _ => false,
        },
        HeaderPattern::Nominal(name, patterns) => match target {
            MetaType::Nominal(target_name, arguments)
                if target_name == name && arguments.len() == patterns.len() =>
            {
                patterns
                    .iter()
                    .zip(arguments)
                    .all(|(pattern, argument)| match_header(pattern, argument, env))
            }
            _ => false,
        },
        HeaderPattern::Slotted(patterns, tail) => match target {
            MetaType::SlottedProduct(elements) => match_elements(
                patterns,
                tail.as_deref(),
                elements,
                MetaType::SlottedProduct,
                env,
            ),
            _ => false,
        },
        HeaderPattern::Product(patterns, tail) => match target {
            MetaType::Product(elements) => {
                match_elements(patterns, tail.as_deref(), elements, MetaType::Product, env)
            }
            _ => false,
        },
    }
}

fn match_elements(
    patterns: &[HeaderPattern],
    tail: Option<&str>,
    elements: &[MetaType],
    wrap: fn(Vec<MetaType>) -> MetaType,
    env: &mut HashMap<String, TypeBinding>,
) -> bool {
    if elements.len() < patterns.len() || (tail.is_none() && elements.len() != patterns.len()) {
        return false;
    }
    patterns
        .iter()
        .zip(elements)
        .all(|(pattern, element)| match_header(pattern, element, env))
        && tail.is_none_or(|name| {
            bind_parameter(
                env,
                name,
                TypeBinding::Meta(wrap(elements[patterns.len()..].to_vec())),
            )
        })
}

/// Writes a header (or any type) as a parse target, substituting what the
/// enclosing impl's parameters stand for.
pub(super) fn instantiate_header(
    pattern: &HeaderPattern,
    env: &HashMap<String, TypeBinding>,
) -> Option<MetaType> {
    let elements = |patterns: &[HeaderPattern], tail: &Option<String>| {
        let mut elements = patterns
            .iter()
            .map(|pattern| instantiate_header(pattern, env))
            .collect::<Option<Vec<_>>>()?;
        if let Some(tail) = tail {
            match env.get(tail)? {
                TypeBinding::Meta(MetaType::SlottedProduct(rest) | MetaType::Product(rest)) => {
                    elements.extend(rest.iter().cloned());
                }
                _ => return None,
            }
        }
        Some(elements)
    };
    Some(match pattern {
        HeaderPattern::Exact(meta) => meta.clone(),
        HeaderPattern::Parameter(name) => match env.get(name)? {
            TypeBinding::Meta(meta) => meta.clone(),
            TypeBinding::Spelling(_) => return None,
        },
        HeaderPattern::Ident(IdentPattern::Any) => MetaType::Ident(None),
        HeaderPattern::Ident(IdentPattern::Spelling(spelling)) => {
            MetaType::Ident(Some(spelling.clone()))
        }
        HeaderPattern::Ident(IdentPattern::Parameter(name)) => match env.get(name)? {
            TypeBinding::Spelling(spelling) => MetaType::Ident(spelling.clone()),
            TypeBinding::Meta(_) => return None,
        },
        HeaderPattern::Optional(inner) => {
            MetaType::Optional(Box::new(instantiate_header(inner, env)?))
        }
        HeaderPattern::Sequence(inner) => {
            MetaType::Sequence(Box::new(instantiate_header(inner, env)?))
        }
        HeaderPattern::Delimited(kind, inner) => {
            MetaType::Delimited(*kind, meta_contents(instantiate_header(inner, env)?))
        }
        HeaderPattern::Nominal(name, patterns) => MetaType::Nominal(
            name.clone(),
            patterns
                .iter()
                .map(|pattern| instantiate_header(pattern, env))
                .collect::<Option<Vec<_>>>()?,
        ),
        HeaderPattern::Slotted(patterns, tail) => {
            MetaType::SlottedProduct(elements(patterns, tail)?)
        }
        HeaderPattern::Product(patterns, tail) => MetaType::Product(elements(patterns, tail)?),
    })
}

/// The `Parse` target a type names, with `env` supplying the enclosing impl's
/// parameters.
pub(super) fn parse_target_meta(ty: &Type, env: &HashMap<String, TypeBinding>) -> Option<MetaType> {
    let parameters = env.keys().cloned().collect::<HashSet<_>>();
    instantiate_header(&header_pattern(ty, &parameters)?, env)
}

/// The `Parse` target a statically inferred compile-time type stands for.
pub(super) fn compile_type_target(
    ty: &CompileType,
    env: &HashMap<String, TypeBinding>,
) -> Option<MetaType> {
    match ty {
        CompileType::Meta(meta) => Some(meta.clone()),
        CompileType::Named(name) => match env.get(name) {
            Some(TypeBinding::Meta(meta)) => Some(meta.clone()),
            Some(TypeBinding::Spelling(_)) => None,
            None => Some(MetaType::Nominal(name.clone(), Vec::new())),
        },
        CompileType::Written(ty) => parse_target_meta(ty, env),
        CompileType::Product(elements) => elements
            .iter()
            .map(|element| compile_type_target(element, env))
            .collect::<Option<Vec<_>>>()
            .map(MetaType::Product),
        _ => None,
    }
}

/// A string value as the interpreter represents it: the quoted literal.
pub(super) fn string_value(text: &str) -> Value {
    Value::String(staple_syntax::string_literal::encode(text))
}

pub(super) fn parse_error_value(message: &str) -> Value {
    Value::Nominal(
        "ParseError".to_owned(),
        Box::new(Value::Product(
            vec![(Some("message".to_owned()), string_value(message))].into(),
            false,
        )),
    )
}

pub(super) fn ok_value(value: Value) -> Value {
    Value::Nominal("Ok".to_owned(), Box::new(value))
}

/// Where the first unread token of a stream is, for diagnostics.
fn describe_position(syntax: &Syntax, cursor: usize) -> String {
    let tokens = syntax.tokens();
    let Some(offset) = (cursor..tokens.len()).find(|index| !tokens[*index].kind.is_trivia()) else {
        return "end of input".to_owned();
    };
    let position = syntax.token_range.start + offset;
    let (mut line, mut column) = (1usize, 1usize);
    for token in &syntax.token_stream()[..position] {
        for character in token.text.chars() {
            if character == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
    }
    format!("line {line}, column {column}")
}

/// The syntax a value stands for as a whole, for `stream` and `parse_all`.
pub(super) fn value_syntax(value: &Value) -> Option<Syntax> {
    match value {
        Value::Product(fields, false) if fields.len() == 1 && fields[0].0.is_none() => {
            value_syntax(&fields[0].1)
        }
        Value::Syntax(value) => value.syntax().cloned(),
        _ => None,
    }
}

impl MacroExpander {
    /// Whether `named` (written in `module`) names the `Parse` trait.
    pub(super) fn names_parse_trait(
        &self,
        module: ModuleId,
        named: &staple_syntax::NamedType,
    ) -> bool {
        let helper = match &named.namespace {
            None => self.lookup_helper(module, &named.name),
            Some(namespace) => self.scopes[module.0]
                .namespaces
                .get(namespace)
                .and_then(|target| self.scopes[target.0].helpers.get(&named.name))
                .cloned(),
        };
        helper.is_some_and(|helper| self.is_parse_trait(&helper))
    }

    /// Whether `item` exists only for compile-time parsing: a `Parse`
    /// implementation, or the block declaring the `std.syntax` intrinsics.
    pub(super) fn is_compile_time_parse_item(&self, module: ModuleId, item: &Item) -> bool {
        match item {
            Item::TraitImplementation(implementation) => {
                self.names_parse_trait(module, &implementation.trait_name)
                    || implementation.arguments.iter().any(type_contains_syntax)
            }
            Item::ExternBlock(block) => {
                Some(module) == self.syntax_module && block.abi == "\"staple-intrinsic\""
            }
            Item::Modified(modified) => self.is_compile_time_parse_item(module, &modified.item),
            _ => false,
        }
    }

    pub(super) fn is_parse_trait(&self, helper: &HelperDefinition) -> bool {
        Some(helper.module) == self.syntax_module
            && helper.binding.kind == BindingKind::Extern
            && helper.binding.name == "Parse"
    }

    /// Finds a helper by name from `module`, the way a bare name is resolved.
    pub(super) fn lookup_helper(&self, module: ModuleId, name: &str) -> Option<HelperDefinition> {
        let mut cursor = module;
        loop {
            if let Some(helper) = self.scopes[cursor.0].helpers.get(name) {
                return Some(helper.clone());
            }
            if !self.companion_modules[cursor.0] {
                return None;
            }
            cursor = self.parent_modules[cursor.0]?;
        }
    }

    /// The compiler-implemented function a helper definition stands for.
    pub(super) fn helper_intrinsic(&self, helper: &HelperDefinition) -> Option<SyntaxIntrinsic> {
        if Some(helper.module) == self.syntax_module && helper.binding.kind == BindingKind::Extern {
            SyntaxIntrinsic::from_name(&helper.binding.name)
        } else if helper.binding.kind == BindingKind::Extern
            && self
                .float_companion_modules
                .iter()
                .any(|module| *module == Some(helper.module))
        {
            SyntaxIntrinsic::from_float_name(&helper.binding.name)
        } else if helper.binding.kind == BindingKind::Extern
            && self
                .integer_companion_modules
                .iter()
                .any(|module| *module == Some(helper.module))
        {
            SyntaxIntrinsic::from_integer_name(&helper.binding.name)
        } else {
            None
        }
    }

    /// Recognizes a `Parse` member head: `Parse.parse`, `syntax.Parse.parse_all`,
    /// or a bare `parse` when the trait is in scope, optionally applied to a
    /// type argument with `.<T>`.
    pub(super) fn parse_member_head<'a>(
        &self,
        module: ModuleId,
        head: &'a Expression,
        local: impl Fn(&str) -> bool,
    ) -> Option<(ParseMember, Option<&'a [Type]>)> {
        let (head, arguments) = match head {
            Expression::TypeApplication(application) => (
                application.value.as_ref(),
                Some(application.arguments.as_slice()),
            ),
            head => (head, None),
        };
        let member_of = |name: &str| match name {
            "parse" => Some(ParseMember::Parse),
            "parse_all" => Some(ParseMember::ParseAll),
            _ => None,
        };
        match head {
            Expression::Access(access) => {
                let Accessor::Name(member) = &access.accessor else {
                    return None;
                };
                let member = member_of(member)?;
                let is_trait = match access.value.as_ref() {
                    Expression::Name(name) => {
                        !local(&name.name)
                            && self
                                .lookup_helper(
                                    name.syntax
                                        .definition_module()
                                        .map(ModuleId)
                                        .unwrap_or(module),
                                    &name.name,
                                )
                                .is_some_and(|helper| self.is_parse_trait(&helper))
                    }
                    Expression::Access(inner) => {
                        let Accessor::Name(item) = &inner.accessor else {
                            return None;
                        };
                        let (namespace, definition_module) = match inner.value.as_ref() {
                            Expression::Name(name) => {
                                (name.name.clone(), name.syntax.definition_module())
                            }
                            Expression::Access(path) => {
                                let (namespace, last, definition_module) =
                                    qualified_macro_access_path(path)?;
                                (format!("{namespace}.{last}"), definition_module)
                            }
                            _ => return None,
                        };
                        let context = definition_module.map(ModuleId).unwrap_or(module);
                        self.scopes[context.0]
                            .namespaces
                            .get(&namespace)
                            .and_then(|target| self.scopes[target.0].helpers.get(item))
                            .is_some_and(|helper| self.is_parse_trait(helper))
                    }
                    _ => false,
                };
                is_trait.then_some((member, arguments))
            }
            Expression::Name(name) => {
                let member = member_of(&name.name)?;
                if local(&name.name) {
                    return None;
                }
                let context = name
                    .syntax
                    .definition_module()
                    .map(ModuleId)
                    .unwrap_or(module);
                if self.lookup_helper(context, &name.name).is_some() {
                    return None;
                }
                let in_scope = self.scopes[context.0]
                    .helpers
                    .values()
                    .any(|helper| self.is_parse_trait(helper));
                in_scope.then_some((member, arguments))
            }
            _ => None,
        }
    }

    fn advance_stream(&mut self, place: &Option<EnvironmentBinding>, value: Value) {
        if let Some(place) = place {
            *place.value.borrow_mut() = value;
        }
    }

    /// Runs a compiler-implemented `std.syntax` function. `place` is the
    /// caller's storage for a `mut` argument.
    pub(super) fn apply_intrinsic(
        &mut self,
        intrinsic: SyntaxIntrinsic,
        type_argument: Option<MetaType>,
        argument: Value,
        place: Option<EnvironmentBinding>,
        span: Span,
    ) -> Option<Value> {
        if intrinsic.takes_type_argument() && type_argument.is_none() {
            self.diagnostics.push(Diagnostic::new(
                span,
                "this function requires an explicit type argument",
            ));
            return None;
        }
        match intrinsic {
            SyntaxIntrinsic::DuplicateModifier => {
                let Value::Syntax(SyntaxValue::Modifier(modifier)) = argument else {
                    return None;
                };
                let mut duplicate = || {
                    let invocation = if modifier.invocation.doc.is_some() {
                        let mut invocation = modifier.invocation.clone();
                        invocation.syntax =
                            freshened_syntax(&invocation.syntax, &mut self.next_syntax_id);
                        invocation
                    } else {
                        staple_syntax::parse_modifier_fragment(
                            &modifier.invocation.syntax,
                            &mut self.next_syntax_id,
                        )
                        .ok()?
                    };
                    Some((
                        None,
                        Value::Syntax(SyntaxValue::Modifier(OpaqueModifier {
                            invocation,
                            enclosing: None,
                        })),
                    ))
                };
                Some(Value::Product(
                    vec![duplicate()?, duplicate()?].into(),
                    false,
                ))
            }
            SyntaxIntrinsic::Stream => {
                let Some(syntax) = value_syntax(&argument) else {
                    self.diagnostics
                        .push(Diagnostic::new(span, "`stream` requires `Syntax`"));
                    return None;
                };
                Some(Value::Stream { syntax, cursor: 0 })
            }
            SyntaxIntrinsic::IsEmpty => {
                let (syntax, cursor) = self.stream_argument(argument, span)?;
                Some(bool_value(
                    syntax.tokens()[cursor..]
                        .iter()
                        .all(|token| token.kind.is_trivia()),
                ))
            }
            SyntaxIntrinsic::Remaining => {
                let (syntax, cursor) = self.stream_argument(argument, span)?;
                let end = syntax.tokens().len();
                Some(Value::Syntax(SyntaxValue::Raw(syntax_slice(
                    &syntax, cursor, end,
                ))))
            }
            SyntaxIntrinsic::SequencePush => {
                let Value::Product(mut values, _) = argument else {
                    return None;
                };
                if values.len() != 2 {
                    return None;
                }
                let item = values.pop()?.1;
                let Value::Sequence(mut sequence) = values.pop()?.1 else {
                    return None;
                };
                sequence.push(item);
                Some(Value::Sequence(sequence))
            }
            SyntaxIntrinsic::Offset => {
                let (_, cursor) = self.stream_argument(argument, span)?;
                Some(Value::Integer(cursor as i128, None))
            }
            SyntaxIntrinsic::Float(intrinsic) => {
                self.eval_float_intrinsic(intrinsic, argument, span)
            }
            SyntaxIntrinsic::Integer(intrinsic) => {
                self.eval_integer_intrinsic(intrinsic, argument, span)
            }
            SyntaxIntrinsic::ErrorAt => {
                let Value::Product(mut elements, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`error_at` requires `(ParseStream; String)`",
                    ));
                    return None;
                };
                if elements.len() != 2 {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`error_at` requires `(ParseStream; String)`",
                    ));
                    return None;
                }
                let (_, message) = elements.remove(1);
                let (_, stream) = elements.remove(0);
                let (syntax, cursor) = self.stream_argument(stream, span.clone())?;
                let Value::String(message) = message else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "`error_at` requires `(ParseStream; String)`",
                    ));
                    return None;
                };
                let message = staple_syntax::string_literal::decode(&message).unwrap_or(message);
                Some(parse_error_value(&format!(
                    "{message} at {}",
                    describe_position(&syntax, cursor)
                )))
            }
            SyntaxIntrinsic::TakeRest => {
                let (syntax, cursor) = self.stream_argument(argument, span)?;
                let end = syntax.tokens().len();
                let rest = syntax_slice(&syntax, cursor, end);
                self.advance_stream(
                    &place,
                    Value::Stream {
                        syntax,
                        cursor: end,
                    },
                );
                Some(Value::Syntax(SyntaxValue::Raw(rest)))
            }
            SyntaxIntrinsic::Group(kind) => {
                let (syntax, cursor) = self.stream_argument(argument, span)?;
                let tokens = syntax.tokens();
                let (open, close_kind, name) = match kind {
                    DelimiterKind::Parenthesized => (
                        staple_syntax::TokenKind::LParen,
                        staple_syntax::TokenKind::RParen,
                        "parenthesized",
                    ),
                    DelimiterKind::Bracketed => (
                        staple_syntax::TokenKind::LBracket,
                        staple_syntax::TokenKind::RBracket,
                        "bracketed",
                    ),
                    DelimiterKind::Angled => (
                        staple_syntax::TokenKind::Operator,
                        staple_syntax::TokenKind::Operator,
                        "angle-bracketed",
                    ),
                    DelimiterKind::Braced => (
                        staple_syntax::TokenKind::LBrace,
                        staple_syntax::TokenKind::RBrace,
                        "braced",
                    ),
                };
                let first = skip_trivia(tokens, cursor, tokens.len());
                let close = (first < tokens.len() && tokens[first].kind == open)
                    .then(|| {
                        if kind == DelimiterKind::Angled {
                            (tokens[first].text == "<")
                                .then(|| matching_angle_close(tokens, first))
                                .flatten()
                        } else {
                            matching_close(tokens, first)
                        }
                    })
                    .flatten()
                    .filter(|close| tokens[*close].kind == close_kind);
                let Some(close) = close else {
                    return Some(parse_error_value(&format!(
                        "expected {name} syntax at {}",
                        describe_position(&syntax, cursor)
                    )));
                };
                let source = syntax_slice(&syntax, first, close + 1);
                let inner = syntax_slice(&syntax, first + 1, close);
                self.advance_stream(
                    &place,
                    Value::Stream {
                        syntax,
                        cursor: close + 1,
                    },
                );
                Some(ok_value(Value::Product(
                    vec![
                        (None, Value::Syntax(SyntaxValue::Raw(source))),
                        (
                            None,
                            Value::Stream {
                                syntax: inner,
                                cursor: 0,
                            },
                        ),
                    ]
                    .into(),
                    true,
                )))
            }
            SyntaxIntrinsic::Delimited(kind) => {
                let target = type_argument?;
                let Value::Product(mut elements, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "a delimiter constructor requires `(Syntax; contents)`",
                    ));
                    return None;
                };
                if elements.len() != 2 {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "a delimiter constructor requires `(Syntax; contents)`",
                    ));
                    return None;
                }
                let (_, value) = elements.remove(1);
                let (_, source) = elements.remove(0);
                let Some(source) = value_syntax(&source) else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "a delimiter constructor requires `(Syntax; contents)`",
                    ));
                    return None;
                };
                let contents = match (&target, value) {
                    (MetaType::SlottedProduct(_), Value::Product(values, _)) => {
                        DelimitedValueContents::Slotted(values.to_vec())
                    }
                    (MetaType::Product(_), Value::Product(values, _)) if values.len() >= 2 => {
                        DelimitedValueContents::Product(values.to_vec())
                    }
                    (MetaType::Product(_), Value::Product(values, _)) => {
                        DelimitedValueContents::Slotted(values.to_vec())
                    }
                    (MetaType::Sequence(_), Value::Sequence(values)) => {
                        DelimitedValueContents::Sequence(values)
                    }
                    (
                        _,
                        Value::Separated {
                            elements,
                            separator,
                            trailing,
                        },
                    ) => DelimitedValueContents::Separated {
                        elements,
                        separator,
                        trailing,
                    },
                    (_, value) => DelimitedValueContents::Slotted(vec![(None, value)]),
                };
                let expression =
                    staple_syntax::parse_expression_fragment(&source, &mut self.next_syntax_id)
                        .ok()
                        .map(Box::new);
                Some(Value::Syntax(SyntaxValue::Delimited(
                    DelimitedSyntaxValue {
                        kind,
                        syntax: source,
                        contents,
                        generated: Vec::new(),
                        expression,
                    },
                )))
            }
            SyntaxIntrinsic::ParsePrimitive => {
                let target = type_argument?;
                let (syntax, cursor) = self.stream_argument(argument, span.clone())?;
                self.parse_primitive(&target, syntax, cursor, place, span)
            }
        }
    }

    fn stream_argument(&mut self, argument: Value, span: Span) -> Option<(Syntax, usize)> {
        match argument {
            Value::Stream { syntax, cursor } => Some((syntax, cursor)),
            _ => {
                self.diagnostics
                    .push(Diagnostic::new(span, "expected a `ParseStream`"));
                None
            }
        }
    }

    /// Reads one leaf node of type `target` from the front of a stream. A
    /// syntax error is a `ParseError` result that leaves the stream alone.
    fn parse_primitive(
        &mut self,
        target: &MetaType,
        syntax: Syntax,
        cursor: usize,
        place: Option<EnvironmentBinding>,
        span: Span,
    ) -> Option<Value> {
        let end = syntax.tokens().len();
        let rest = syntax_slice(&syntax, cursor, end);
        let tokens = rest.tokens();
        let first = skip_trivia(tokens, 0, tokens.len());
        let fail = |message: Option<String>| {
            let mut text = format!("expected {}", format_meta_type(target));
            if let Some(message) = message {
                text.push_str(": ");
                text.push_str(&message);
            }
            text.push_str(&format!(" at {}", describe_position(&syntax, cursor)));
            Some(parse_error_value(&text))
        };
        let token_slice = |from: usize, to: usize| syntax_slice(&rest, from, to);
        // The node, and how many of the rest's tokens it used.
        let (value, consumed): (Value, usize) = match target {
            MetaType::Expr
            | MetaType::CallExpr
            | MetaType::CharExpr
            | MetaType::StringExpr
            | MetaType::UnstructuredExpr => {
                match staple_syntax::parse_expression_prefix(&rest, &mut self.next_syntax_id) {
                    Ok((expression, consumed)) => {
                        let value = SyntaxValue::from_expression(expression);
                        let accepted = match target {
                            MetaType::CallExpr => matches!(value, SyntaxValue::Call(_)),
                            MetaType::CharExpr => {
                                matches!(value, SyntaxValue::Unstructured(Expression::Char(_)))
                            }
                            MetaType::StringExpr => {
                                matches!(value, SyntaxValue::Unstructured(Expression::String(_)))
                            }
                            MetaType::UnstructuredExpr => {
                                matches!(value, SyntaxValue::Unstructured(_))
                            }
                            _ => true,
                        };
                        if !accepted {
                            return fail(None);
                        }
                        (Value::Syntax(value), consumed)
                    }
                    Err(error) => return fail(Some(error.message)),
                }
            }
            MetaType::Type | MetaType::ProductType | MetaType::UnstructuredType => {
                match staple_syntax::parse_type_prefix(&rest, &mut self.next_syntax_id) {
                    Ok((ty, _)) if !type_category_matches(target, &ty) => return fail(None),
                    Ok((ty, consumed)) => (Value::Syntax(SyntaxValue::Type(ty)), consumed),
                    Err(error) => return fail(Some(error.message)),
                }
            }
            MetaType::Pattern | MetaType::BindingPattern | MetaType::NominalPattern => {
                match staple_syntax::parse_pattern_prefix(&rest, &mut self.next_syntax_id) {
                    Ok((pattern, consumed)) => {
                        if matches!(target, MetaType::BindingPattern)
                            && !matches!(pattern, Pattern::Binding(_))
                            || matches!(target, MetaType::NominalPattern)
                                && !matches!(pattern, Pattern::Nominal(_))
                        {
                            return fail(None);
                        }
                        (Value::Syntax(SyntaxValue::Pattern(pattern)), consumed)
                    }
                    Err(error) => return fail(Some(error.message)),
                }
            }
            MetaType::Item
            | MetaType::ModifiedItem
            | MetaType::TypeDeclarationItem
            | MetaType::UnstructuredItem => {
                match staple_syntax::parse_item_prefix(&rest, &mut self.next_syntax_id) {
                    Ok((item, consumed)) => {
                        if matches!(target, MetaType::ModifiedItem)
                            && !matches!(item, Item::Modified(_))
                            || matches!(target, MetaType::TypeDeclarationItem)
                                && !matches!(item, Item::TypeDeclaration(_))
                            || matches!(target, MetaType::UnstructuredItem)
                                && matches!(item, Item::Modified(_) | Item::TypeDeclaration(_))
                        {
                            return fail(None);
                        }
                        (Value::Syntax(SyntaxValue::Item(Box::new(item))), consumed)
                    }
                    Err(error) => return fail(Some(error.message)),
                }
            }
            MetaType::Modifier | MetaType::ModifierCall | MetaType::UnstructuredModifier => {
                match staple_syntax::parse_modifier_prefix(&rest, &mut self.next_syntax_id) {
                    Ok((invocation, _))
                        if matches!(target, MetaType::UnstructuredModifier)
                            && invocation.doc.is_none()
                            || matches!(target, MetaType::ModifierCall)
                                && invocation.doc.is_some() =>
                    {
                        return fail(None);
                    }
                    Ok((invocation, consumed)) => (
                        Value::Syntax(SyntaxValue::Modifier(OpaqueModifier {
                            invocation,
                            enclosing: None,
                        })),
                        consumed,
                    ),
                    Err(error) => return fail(Some(error.message)),
                }
            }
            MetaType::TypeParameter | MetaType::TypeParameters => {
                match parse_parameter_syntax(target, &rest, &mut self.next_syntax_id) {
                    Ok(result) => result,
                    Err(error) => return fail(Some(error.message)),
                }
            }
            MetaType::TypeBody => {
                match staple_syntax::parse_type_body_prefix(&rest, &mut self.next_syntax_id) {
                    Ok((body, consumed)) => (type_body_value(&body), consumed),
                    Err(error) => return fail(Some(error.message)),
                }
            }
            MetaType::Ident(spelling) => {
                let token = tokens.get(first);
                match token {
                    Some(token)
                        if token.kind == staple_syntax::TokenKind::Identifier
                            && spelling
                                .as_ref()
                                .is_none_or(|expected| expected == &token.text) =>
                    {
                        (
                            Value::Syntax(SyntaxValue::Ident(staple_syntax::NameExpression {
                                syntax: token_slice(first, first + 1),
                                name: token.text.clone(),
                            })),
                            first + 1,
                        )
                    }
                    _ => return fail(None),
                }
            }
            MetaType::Comma | MetaType::Equals | MetaType::FatArrow => {
                let kind = match target {
                    MetaType::Comma => staple_syntax::TokenKind::Comma,
                    MetaType::Equals => staple_syntax::TokenKind::Equals,
                    _ => staple_syntax::TokenKind::FatArrow,
                };
                if tokens.get(first).is_none_or(|token| token.kind != kind) {
                    return fail(None);
                }
                let syntax = token_slice(first, first + 1);
                (
                    Value::Syntax(match target {
                        MetaType::Comma => SyntaxValue::Comma(syntax),
                        MetaType::Equals => SyntaxValue::Equals(syntax),
                        _ => SyntaxValue::FatArrow(syntax),
                    }),
                    first + 1,
                )
            }
            MetaType::Visibility => {
                use staple_syntax::TokenKind;
                if tokens
                    .get(first)
                    .is_some_and(|token| token.kind == TokenKind::Pub)
                {
                    let mut end = first + 1;
                    let mut kind = VisibilityKind::Public;
                    let next = skip_trivia(tokens, end, tokens.len());
                    if tokens
                        .get(next)
                        .is_some_and(|token| token.kind == TokenKind::LParen)
                        && let Some(close) = matching_close(tokens, next)
                    {
                        let inner = tokens[next + 1..close]
                            .iter()
                            .filter(|token| !token.kind.is_trivia())
                            .collect::<Vec<_>>();
                        if matches!(inner.as_slice(), [token] if token.kind == TokenKind::Package) {
                            kind = VisibilityKind::Package;
                            end = close + 1;
                        }
                    }
                    (
                        Value::Syntax(SyntaxValue::Visibility(VisibilitySyntax {
                            syntax: token_slice(first, end),
                            kind,
                        })),
                        end,
                    )
                } else {
                    // Nothing written means private; nothing is consumed.
                    (
                        Value::Syntax(SyntaxValue::Visibility(private_visibility())),
                        0,
                    )
                }
            }
            MetaType::SyntaxNode => {
                let Some(token) = tokens.get(first) else {
                    return fail(None);
                };
                let end = match token.kind {
                    staple_syntax::TokenKind::LParen
                    | staple_syntax::TokenKind::LBracket
                    | staple_syntax::TokenKind::LBrace => match matching_close(tokens, first) {
                        Some(close) => close + 1,
                        None => return fail(None),
                    },
                    _ => first + 1,
                };
                let atom = token_slice(first, end);
                match structural_syntax_value(&atom, &mut self.next_syntax_id) {
                    Some(value) => (Value::Syntax(value), end),
                    None => return fail(None),
                }
            }
            other => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "`__parse_primitive` cannot parse `{}`",
                        format_meta_type(other)
                    ),
                ));
                return None;
            }
        };
        // Syntax quoted by a macro resolves where it was quoted, as
        // `parse_quote` does; tokens from a caller keep the caller's context.
        let mut value = value;
        if let Value::Syntax(syntax_value) = &mut value
            && tokens[..consumed.min(tokens.len())]
                .iter()
                .any(|token| token.hygiene.is_some())
        {
            self.freshen_by_tokens(syntax_value, ModuleId(0), 0);
        }
        self.advance_stream(
            &place,
            Value::Stream {
                syntax,
                cursor: cursor + consumed,
            },
        );
        Some(ok_value(value))
    }

    /// Finds the single `Parse` implementation for `target`.
    fn select_parse_impl(&mut self, target: &MetaType, span: Span) -> Option<(usize, TypeEnv)> {
        let mut matches = Vec::new();
        for (index, implementation) in self.parse_impls.iter().enumerate() {
            let mut env = HashMap::new();
            if match_header(&implementation.header, target, &mut env) {
                matches.push((index, env));
            }
        }
        match matches.len() {
            0 => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("`{}` does not implement `Parse`", format_meta_type(target)),
                ));
                None
            }
            1 => {
                let (index, env) = matches.remove(0);
                Some((index, Rc::new(env)))
            }
            _ => {
                let names = matches
                    .iter()
                    .map(|(index, _)| {
                        format!("`impl Parse {}`", self.parse_impls[*index].header_text)
                    })
                    .collect::<Vec<_>>()
                    .join(" and ");
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "`{}` has ambiguous `Parse` implementations: {names}",
                        format_meta_type(target)
                    ),
                ));
                None
            }
        }
    }

    /// The caller's storage named by a `mut` argument, if it names any.
    fn argument_place_of(
        &mut self,
        expression: &Expression,
        environment: &Environment,
    ) -> Option<Option<EnvironmentBinding>> {
        let expression = match expression {
            Expression::Product(product)
                if product.elements.len() == 1
                    && product.elements[0].name.is_none()
                    && !product.elements[0].spread =>
            {
                &product.elements[0].value
            }
            expression => expression,
        };
        let Expression::Name(name) = expression else {
            return Some(None);
        };
        let Some(binding) = environment.get(&name.name) else {
            return Some(None);
        };
        if !binding.mutable {
            self.diagnostics.push(Diagnostic::new(
                name.syntax.span.clone(),
                format!(
                    "cannot write through `{}`; its binding is not declared `mut`",
                    name.name
                ),
            ));
            return None;
        }
        Some(Some(binding.clone()))
    }

    /// Evaluates a call's argument. A `mut` parameter receives the storage the
    /// argument names, so the callee updates the caller's own binding.
    pub(super) fn eval_call_argument(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        environment: &mut Environment,
        wants_place: bool,
    ) -> Option<(Value, Option<EnvironmentBinding>)> {
        if wants_place && let Some(place) = self.argument_place_of(expression, environment)? {
            let value = place.get();
            return Some((value, Some(place)));
        }
        Some((self.eval_expression(module, expression, environment)?, None))
    }

    /// `Parse.parse` and `Parse.parse_all` calls.
    pub(super) fn eval_parse_call(
        &mut self,
        module: ModuleId,
        call: &staple_syntax::CallExpression,
        member: ParseMember,
        type_arguments: Option<&[Type]>,
        environment: &mut Environment,
    ) -> Option<Value> {
        let span = call.syntax.span.clone();
        let target = match type_arguments {
            Some([ty]) => match parse_target_meta(ty, &self.type_env) {
                Some(target) => target,
                None => {
                    self.diagnostics.push(Diagnostic::new(
                        ty.syntax().span.clone(),
                        format!("`{ty}` cannot be a `Parse` target"),
                    ));
                    return None;
                }
            },
            Some(_) => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "`Parse` members take exactly one type argument",
                ));
                return None;
            }
            None => {
                let target = self
                    .parse_targets
                    .get(&call.syntax.id)
                    .and_then(|ty| compile_type_target(ty, &self.type_env));
                match target {
                    Some(target) => target,
                    None => {
                        self.diagnostics.push(Diagnostic::new(
                            span,
                            "`Parse.parse` requires a contextual `Result<T, ParseError>` type or an explicit type argument",
                        ));
                        return None;
                    }
                }
            }
        };
        let (argument, place) = self.eval_call_argument(
            module,
            &call.argument,
            environment,
            member == ParseMember::Parse,
        )?;
        self.eval_parse_member(member, target, argument, place, span)
    }

    /// A call whose callee is a type-applied value; only the generic `std.syntax`
    /// intrinsics support one.
    pub(super) fn eval_type_applied_call(
        &mut self,
        module: ModuleId,
        call: &staple_syntax::CallExpression,
        application: &staple_syntax::TypeApplicationExpression,
        environment: &mut Environment,
    ) -> Option<Value> {
        let span = call.syntax.span.clone();
        let callee = self.eval_expression(module, &application.value, environment)?;
        let (Value::Intrinsic(intrinsic), [ty]) = (&callee, application.arguments.as_slice())
        else {
            let (argument, place) = self.eval_call_argument(
                module,
                &call.argument,
                environment,
                self.callable_takes_mut(&callee),
            )?;
            self.argument_place = place;
            let target = self
                .call_targets
                .get(&call.syntax.id)
                .and_then(|ty| compile_type_target(ty, &self.type_env))
                .or_else(|| self.call_target.clone());
            let previous = std::mem::replace(&mut self.call_target, target);
            let result = self.apply_value(
                Value::TypeApplied(Box::new(callee), application.arguments.clone()),
                argument,
                span,
            );
            self.call_target = previous;
            return result;
        };
        if !intrinsic.takes_type_argument() {
            self.diagnostics.push(Diagnostic::new(
                application.syntax.span.clone(),
                "call-site type arguments are only supported for runtime generic values",
            ));
            return None;
        }
        let Some(target) = parse_target_meta(ty, &self.type_env) else {
            self.diagnostics.push(Diagnostic::new(
                ty.syntax().span.clone(),
                format!("`{ty}` cannot be a `Parse` target"),
            ));
            return None;
        };
        let (argument, place) =
            self.eval_call_argument(module, &call.argument, environment, intrinsic.takes_mut())?;
        self.apply_intrinsic(*intrinsic, Some(target), argument, place, span)
    }

    /// Evaluates `Parse.parse` or `Parse.parse_all` for `target`.
    pub(super) fn eval_parse_member(
        &mut self,
        member: ParseMember,
        target: MetaType,
        argument: Value,
        place: Option<EnvironmentBinding>,
        span: Span,
    ) -> Option<Value> {
        let (index, env) = self.select_parse_impl(&target, span.clone())?;
        let module = self.parse_impls[index].module;
        let name = match member {
            ParseMember::Parse => "parse",
            ParseMember::ParseAll => "parse_all",
        };
        let (module, expression, env) = match self.parse_impls[index].members.get(name).cloned() {
            Some(expression) => (module, expression, env),
            None if member == ParseMember::ParseAll => {
                let (default_module, default) = self.parse_all_default.clone()?;
                // The default member is generic over the trait's own parameter.
                let mut trait_env = HashMap::new();
                trait_env.insert("T".to_owned(), TypeBinding::Meta(target));
                (default_module, default, Rc::new(trait_env))
            }
            None => {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "this `Parse` implementation has no `parse` member",
                ));
                return None;
            }
        };
        let previous_env = std::mem::replace(&mut self.type_env, env);
        let previous_context = self.quote_context.take();
        let function = self.eval_expression(module, &expression, &mut Environment::new());
        self.quote_context = previous_context;
        self.type_env = previous_env;
        let function = function?;
        self.argument_place = place;
        self.apply_value(function, argument, span)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ParseMember {
    Parse,
    ParseAll,
}

/// Collects every `Parse` implementation in the program, and the trait's
/// default `parse_all`.
pub(super) fn collect_parse_impls(
    program: &Program,
    expander: &MacroExpander,
) -> (
    Vec<ParseImpl>,
    Option<(ModuleId, Expression)>,
    Vec<Diagnostic>,
) {
    let mut impls = Vec::new();
    let mut default = None;
    let mut diagnostics = Vec::new();
    let Some(syntax_module) = expander.syntax_module else {
        return (impls, default, diagnostics);
    };
    let standard_package = expander.module_packages[syntax_module.0];
    let key = traits::TraitKey(syntax_module, "Parse".to_owned());
    if let Some(declaration) = expander.trait_registry.declarations.get(&key) {
        default = declaration
            .members
            .iter()
            .find(|member| member.name == "parse_all")
            .and_then(|member| member.default.clone())
            .map(|body| (syntax_module, body));
    }
    for implementation in expander
        .trait_registry
        .implementations
        .get(&key)
        .into_iter()
        .flatten()
    {
        if let Some(parsed) = parse_impl(
            program,
            expander,
            implementation.module,
            &implementation.declaration,
            standard_package,
            &mut diagnostics,
        ) {
            impls.push(parsed);
        }
    }
    (impls, default, diagnostics)
}

fn parse_impl(
    program: &Program,
    expander: &MacroExpander,
    module: ModuleId,
    implementation: &staple_syntax::TraitImplementation,
    standard_package: Option<staple_project::PackageId>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ParseImpl> {
    let span = implementation.syntax.span.clone();
    let [argument] = implementation.arguments.as_slice() else {
        diagnostics.push(Diagnostic::new(
            span,
            "`Parse` takes exactly one type argument",
        ));
        return None;
    };
    let parameters = implementation
        .type_parameters
        .iter()
        .flat_map(|parameter| parameter.names())
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let Some(header) = header_pattern(argument, &parameters) else {
        diagnostics.push(Diagnostic::new(
            argument.syntax().span.clone(),
            format!("`{argument}` cannot be a `Parse` implementation target"),
        ));
        return None;
    };
    let mut members = HashMap::new();
    for member in &implementation.members {
        if !matches!(member.name.as_str(), "parse" | "parse_all") {
            diagnostics.push(Diagnostic::new(
                member.syntax.span.clone(),
                format!("`Parse` has no member `{}`", member.name),
            ));
            return None;
        }
        members.insert(member.name.clone(), member.value.clone());
    }
    if !members.contains_key("parse") {
        diagnostics.push(Diagnostic::new(
            span,
            "a `Parse` implementation must define `parse`",
        ));
        return None;
    }
    // The type checker never sees these implementations, so the package-level
    // orphan rule is enforced here: outside the standard library, the target
    // must be a nominal type declared in the implementing package.
    let package = expander.module_packages[module.0];
    if package != standard_package {
        let local = match &header {
            HeaderPattern::Nominal(name, _) => program.modules().iter().any(|candidate| {
                expander.module_packages[candidate.id.0] == package
                    && candidate.syntax.items.iter().any(|item| {
                        let item = match item {
                            Item::Modified(modified) => modified.item.as_ref(),
                            item => item,
                        };
                        matches!(item, Item::TypeDeclaration(declaration) if &declaration.name == name)
                    })
            }),
            _ => false,
        };
        if !local {
            diagnostics.push(Diagnostic::new(
                span,
                "cannot implement an external trait for external types; the trait or at least one top-level nominal implementation argument must be defined in the current package",
            ));
            return None;
        }
    }
    let parse_bounds = implementation
        .trait_bounds
        .iter()
        .filter(|bound| expander.names_parse_trait(module, &bound.trait_name))
        .filter_map(|bound| match bound.arguments.as_slice() {
            [Type::Named(named)] => Some(named.name.clone()),
            _ => None,
        })
        .collect();
    Some(ParseImpl {
        module,
        header,
        header_text: if matches!(argument, Type::Named(_)) {
            argument.to_string()
        } else {
            format!("({argument})")
        },
        members,
        target: if parameters.is_empty() {
            compile_type(argument)
        } else {
            CompileType::Unknown
        },
        parameters,
        parse_bounds,
    })
}

impl MacroExpander {
    /// Whether the function a call head names takes a `mut` parameter.
    pub(super) fn head_takes_mut(
        &self,
        module: ModuleId,
        head: &Expression,
        environment: &CompileEnvironment,
    ) -> bool {
        let Expression::Name(name) = head else {
            return false;
        };
        if environment.contains_key(&name.name) {
            return false;
        }
        let context = name
            .syntax
            .definition_module()
            .map(ModuleId)
            .unwrap_or(module);
        let Some(helper) = self.lookup_helper(context, &name.name) else {
            return false;
        };
        if let Some(intrinsic) = self.helper_intrinsic(&helper) {
            return intrinsic.takes_mut();
        }
        match &helper.binding.value {
            Some(Expression::Function(function)) => {
                mutable_parameter_name(&function.pattern).is_some()
            }
            _ => matches!(helper.binding.annotation_body(),
                Some(Type::Function(function)) if !function.mutations().is_empty()),
        }
    }

    /// Rejects a `mut` argument that names an immutable binding.
    pub(super) fn require_mutable_argument(
        &mut self,
        argument: &Expression,
        environment: &CompileEnvironment,
    ) {
        let argument = match argument {
            Expression::Product(product)
                if product.elements.len() == 1
                    && product.elements[0].name.is_none()
                    && !product.elements[0].spread =>
            {
                &product.elements[0].value
            }
            argument => argument,
        };
        if let Expression::Name(name) = argument
            && environment
                .get(&name.name)
                .is_some_and(|binding| !binding.mutable)
        {
            self.diagnostics.push(Diagnostic::new(
                name.syntax.span.clone(),
                format!(
                    "cannot write through `{}`; its binding is not declared `mut`",
                    name.name
                ),
            ));
        }
    }

    /// Checks a `Parse` target written as a type argument, returning its
    /// compile-time type.
    fn check_parse_target(&mut self, ty: &Type, span: Span) -> CompileType {
        let Some(pattern) = header_pattern(ty, &self.static_parameters) else {
            self.diagnostics.push(Diagnostic::new(
                ty.syntax().span.clone(),
                format!("`{ty}` cannot be a `Parse` target"),
            ));
            return CompileType::Error;
        };
        if let HeaderPattern::Parameter(name) = &pattern {
            if !self.static_parse_bounds.contains(name) {
                self.diagnostics.push(Diagnostic::new(
                    ty.syntax().span.clone(),
                    format!("`{name}` does not implement `Parse`; add a `Parse {name}` bound"),
                ));
                return CompileType::Error;
            }
        } else if self.static_parameters.is_empty()
            && let Some(target) = instantiate_header(&pattern, &HashMap::new())
        {
            // A concrete target must have exactly one implementation.
            let _ = self.select_parse_impl(&target, span);
        }
        compile_type(ty)
    }

    /// Statically checks `Parse.parse` and `Parse.parse_all`.
    pub(super) fn check_parse_call(
        &mut self,
        module: ModuleId,
        call: &staple_syntax::CallExpression,
        member: ParseMember,
        type_arguments: Option<&[Type]>,
        environment: &mut CompileEnvironment,
        expected: Option<&CompileType>,
    ) -> CompileType {
        let span = call.syntax.span.clone();
        let target = match type_arguments {
            Some([ty]) => self.check_parse_target(ty, span.clone()),
            Some(_) => {
                self.diagnostics.push(Diagnostic::new(
                    span.clone(),
                    "`Parse` members take exactly one type argument",
                ));
                CompileType::Error
            }
            None => match expected {
                Some(CompileType::Result(ok, _))
                    if !matches!(ok.as_ref(), CompileType::Unknown | CompileType::Error) =>
                {
                    self.parse_targets
                        .insert(call.syntax.id, ok.as_ref().clone());
                    if self.static_parameters.is_empty()
                        && let Some(target) = compile_type_target(ok, &HashMap::new())
                    {
                        // A concrete target must have exactly one implementation.
                        let _ = self.select_parse_impl(&target, span.clone());
                    }
                    ok.as_ref().clone()
                }
                _ => {
                    self.diagnostics.push(Diagnostic::new(
                        span.clone(),
                        "`Parse.parse` requires a contextual `Result<T, ParseError>` type or an explicit type argument",
                    ));
                    CompileType::Error
                }
            },
        };
        let argument_type = match member {
            ParseMember::Parse => {
                self.require_mutable_argument(&call.argument, environment);
                CompileType::Stream
            }
            ParseMember::ParseAll => CompileType::Meta(MetaType::Syntax),
        };
        self.check_compile_expression(module, &call.argument, environment, Some(&argument_type));
        if target == CompileType::Error {
            return CompileType::Error;
        }
        CompileType::Result(
            Box::new(target),
            Box::new(CompileType::Named("ParseError".to_owned())),
        )
    }

    /// Statically checks a call through a type argument, which only the
    /// generic `std.syntax` intrinsics accept.
    pub(super) fn check_type_applied_call(
        &mut self,
        module: ModuleId,
        call: &staple_syntax::CallExpression,
        application: &staple_syntax::TypeApplicationExpression,
        environment: &mut CompileEnvironment,
    ) -> CompileType {
        let intrinsic = match application.value.as_ref() {
            Expression::Name(name) if !environment.contains_key(&name.name) => self
                .lookup_helper(
                    name.syntax
                        .definition_module()
                        .map(ModuleId)
                        .unwrap_or(module),
                    &name.name,
                )
                .and_then(|helper| self.helper_intrinsic(&helper)),
            _ => None,
        };
        let (Some(intrinsic), [ty]) = (intrinsic, application.arguments.as_slice()) else {
            self.check_compile_expression(module, &application.value, environment, None);
            self.check_compile_expression(module, &call.argument, environment, None);
            return CompileType::Unknown;
        };
        if !matches!(
            intrinsic,
            SyntaxIntrinsic::ParsePrimitive | SyntaxIntrinsic::Delimited(_)
        ) {
            self.check_compile_expression(module, &application.value, environment, None);
            self.check_compile_expression(module, &call.argument, environment, None);
            return CompileType::Unknown;
        }
        let target = compile_type(ty);
        if intrinsic.takes_mut() {
            self.require_mutable_argument(&call.argument, environment);
            self.check_compile_expression(
                module,
                &call.argument,
                environment,
                Some(&CompileType::Stream),
            );
            CompileType::Result(
                Box::new(target),
                Box::new(CompileType::Named("ParseError".to_owned())),
            )
        } else {
            self.check_compile_expression(module, &call.argument, environment, None);
            CompileType::Unknown
        }
    }

    /// Statically checks the members of every `Parse` implementation against
    /// the trait's signatures instantiated at the implementation's header.
    pub(super) fn check_parse_impls(&mut self) {
        for index in 0..self.parse_impls.len() {
            let module = self.parse_impls[index].module;
            let parameters = self.parse_impls[index].parameters.clone();
            let bounds = self.parse_impls[index].parse_bounds.clone();
            let target = self.parse_impls[index].target.clone();
            let members = self.parse_impls[index]
                .members
                .iter()
                .map(|(name, expression)| (name.clone(), expression.clone()))
                .collect::<Vec<_>>();
            self.static_parameters = parameters;
            self.static_parse_bounds = bounds;
            for (name, expression) in members {
                let result = CompileType::Result(
                    Box::new(target.clone()),
                    Box::new(CompileType::Named("ParseError".to_owned())),
                );
                let argument = if name == "parse" {
                    CompileType::Stream
                } else {
                    CompileType::Meta(MetaType::Syntax)
                };
                let expected = CompileType::Function(Box::new(argument), Box::new(result));
                if name == "parse"
                    && !matches!(&expression, Expression::Function(function)
                        if mutable_parameter_name(&function.pattern).is_some())
                {
                    self.diagnostics.push(Diagnostic::new(
                        expression.syntax().span.clone(),
                        "a `Parse.parse` implementation must declare its parameter `mut`",
                    ));
                    continue;
                }
                self.check_compile_expression(
                    module,
                    &expression,
                    &mut CompileEnvironment::new(),
                    Some(&expected),
                );
            }
            self.static_parameters = HashSet::new();
            self.static_parse_bounds = HashSet::new();
        }
    }
}

impl MacroExpander {
    /// Folds a float companion intrinsic. Each operand must be a float of the
    /// intrinsic's own type, or an untyped literal, which takes that type.
    fn eval_float_intrinsic(
        &mut self,
        intrinsic: crate::IntrinsicFunction,
        argument: Value,
        span: Span,
    ) -> Option<Value> {
        use crate::{FloatType, IntrinsicFunction as Intrinsic};
        let (float, arity) = match intrinsic {
            Intrinsic::FloatUnary { float, .. } | Intrinsic::FloatToBits { float } => (float, 1),
            Intrinsic::FloatBinary { float, .. } => (float, 2),
            Intrinsic::FloatTernary { float, .. } => (float, 3),
            Intrinsic::FloatFromBits { float } => {
                let Value::Integer(bits, _) = argument else {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "float bit-cast operand must be an integer",
                    ));
                    return None;
                };
                let value = match float {
                    FloatType::F32 => widen_f32(f32::from_bits(bits as u32)),
                    FloatType::F64 => f64::from_bits(bits as u64),
                };
                return Some(Value::Float(value, Some(float)));
            }
            _ => unreachable!("float companions declare only float intrinsics"),
        };
        // A multi-slot parameter receives its operands as one product.
        let operands = match argument {
            Value::Product(fields, _) if arity > 1 => {
                fields.into_iter().map(|(_, value)| value).collect()
            }
            argument => vec![argument],
        };
        if operands.len() != arity {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("float intrinsic expects {arity} operands"),
            ));
            return None;
        }
        let mut values = Vec::with_capacity(arity);
        for operand in operands {
            match operand {
                Value::Float(value, None) => values.push(round_float(float, value)),
                Value::Float(value, Some(actual)) if actual == float => values.push(value),
                Value::Float(_, Some(actual)) => {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        format!("expected `{}`, found `{}`", float.name(), actual.name()),
                    ));
                    return None;
                }
                _ => {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        "float intrinsic operands must be floats",
                    ));
                    return None;
                }
            }
        }
        let value = match intrinsic {
            Intrinsic::FloatUnary { operation, .. } => {
                apply_float_unary(float, operation, values[0])
            }
            Intrinsic::FloatBinary { operation, .. } => {
                apply_float_binary(float, operation, values[0], values[1])
            }
            Intrinsic::FloatTernary { operation, .. } => {
                apply_float_ternary(float, operation, values[0], values[1], values[2])
            }
            Intrinsic::FloatToBits { .. } => {
                return Some(Value::Integer(
                    match float {
                        FloatType::F32 => i128::from(narrow_to_f32(values[0]).to_bits()),
                        FloatType::F64 => i128::from(values[0].to_bits()),
                    },
                    None,
                ));
            }
            _ => unreachable!("handled above"),
        };
        Some(Value::Float(value, Some(float)))
    }

    /// Folds an integer companion intrinsic at the width the intrinsic names,
    /// even though compile-time integers otherwise have no width.
    fn eval_integer_intrinsic(
        &mut self,
        intrinsic: crate::IntrinsicFunction,
        argument: Value,
        span: Span,
    ) -> Option<Value> {
        use crate::IntrinsicFunction as Intrinsic;
        let (integer, arity) = match intrinsic {
            Intrinsic::IntegerOverflowing { integer, .. } => (integer, 2),
            Intrinsic::IntegerCount { integer, .. } => (integer, 1),
            Intrinsic::IntegerUnary { integer, .. } => (integer, 1),
            Intrinsic::IntegerSignCast { integer } => (integer, 1),
            Intrinsic::IntegerRotate { integer, .. } => (integer, 2),
            _ => unreachable!("integer companions declare only integer intrinsics"),
        };
        let Some(width) = integer.fixed_width() else {
            self.diagnostics.push(Diagnostic::new(
                span,
                "cannot fold a pointer-width integer operation",
            ));
            return None;
        };
        // A multi-slot parameter receives its operands as one product.
        let operands = match argument {
            Value::Product(fields, _) if arity > 1 => {
                fields.into_iter().map(|(_, value)| value).collect()
            }
            argument => vec![argument],
        };
        if operands.len() != arity {
            self.diagnostics.push(Diagnostic::new(
                span,
                format!("integer intrinsic expects {arity} operands"),
            ));
            return None;
        }
        let mut values = Vec::with_capacity(arity);
        for operand in operands {
            let Value::Integer(value, _) = operand else {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    "integer intrinsic operands must be integers",
                ));
                return None;
            };
            values.push(value);
        }
        match intrinsic {
            Intrinsic::IntegerOverflowing { operation, .. } => {
                let (value, overflow) =
                    fold_integer_overflow(integer, width, operation, values[0], values[1]);
                Some(Value::Product(
                    vec![
                        (None, Value::Integer(i128::from(u8::from(overflow)), None)),
                        (None, Value::Integer(value, None)),
                    ]
                    .into(),
                    false,
                ))
            }
            Intrinsic::IntegerCount { operation, .. } => {
                let value = fold_integer_count(integer, width, operation, values[0]);
                Some(Value::Integer(i128::from(value), None))
            }
            Intrinsic::IntegerUnary { operation, .. } => {
                let value = fold_integer_unary(integer, width, operation, values[0]);
                Some(Value::Integer(value, None))
            }
            Intrinsic::IntegerSignCast { .. } => {
                let counterpart = integer.sign_counterpart();
                let value = truncate_integer(integer, width, values[0]);
                let value = truncate_integer(counterpart, width, value);
                Some(Value::Integer(value, Some(counterpart)))
            }
            Intrinsic::IntegerRotate { direction, .. } => {
                let value = fold_integer_rotate(integer, width, direction, values[0], values[1]);
                Some(Value::Integer(value, None))
            }
            _ => unreachable!("handled above"),
        }
    }
}

fn fold_integer_overflow(
    integer: crate::IntegerType,
    width: u32,
    operation: crate::IntegerOverflowOperation,
    left: i128,
    right: i128,
) -> (i128, bool) {
    use crate::IntegerOverflowOperation;
    let left = truncate_integer(integer, width, left);
    let right = truncate_integer(integer, width, right);
    macro_rules! run {
        ($ty:ty) => {{
            let left = left as $ty;
            let right = right as $ty;
            let (value, overflow) = match operation {
                IntegerOverflowOperation::Add => left.overflowing_add(right),
                IntegerOverflowOperation::Subtract => left.overflowing_sub(right),
                IntegerOverflowOperation::Multiply => left.overflowing_mul(right),
            };
            (i128::from(value), overflow)
        }};
    }
    match integer {
        crate::IntegerType::I8 => run!(i8),
        crate::IntegerType::I16 => run!(i16),
        crate::IntegerType::I32 => run!(i32),
        crate::IntegerType::I64 => run!(i64),
        crate::IntegerType::U8 => run!(u8),
        crate::IntegerType::U16 => run!(u16),
        crate::IntegerType::U32 => run!(u32),
        crate::IntegerType::U64 => run!(u64),
        crate::IntegerType::ISize | crate::IntegerType::USize => {
            unreachable!("pointer-width integers do not fold")
        }
    }
}

fn fold_integer_count(
    integer: crate::IntegerType,
    width: u32,
    operation: crate::IntegerCountOperation,
    value: i128,
) -> u32 {
    use crate::IntegerCountOperation;
    let value = truncate_integer(integer, width, value);
    macro_rules! run {
        ($ty:ty) => {{
            let value = value as $ty;
            match operation {
                IntegerCountOperation::CountOnes => value.count_ones(),
                IntegerCountOperation::LeadingZeros => value.leading_zeros(),
                IntegerCountOperation::TrailingZeros => value.trailing_zeros(),
            }
        }};
    }
    match integer {
        crate::IntegerType::I8 => run!(i8),
        crate::IntegerType::I16 => run!(i16),
        crate::IntegerType::I32 => run!(i32),
        crate::IntegerType::I64 => run!(i64),
        crate::IntegerType::U8 => run!(u8),
        crate::IntegerType::U16 => run!(u16),
        crate::IntegerType::U32 => run!(u32),
        crate::IntegerType::U64 => run!(u64),
        crate::IntegerType::ISize | crate::IntegerType::USize => {
            unreachable!("pointer-width integers do not fold")
        }
    }
}

fn fold_integer_unary(
    integer: crate::IntegerType,
    width: u32,
    operation: crate::IntegerUnaryOperation,
    value: i128,
) -> i128 {
    use crate::IntegerUnaryOperation;
    let value = truncate_integer(integer, width, value);
    macro_rules! run {
        ($ty:ty) => {{
            let value = value as $ty;
            i128::from(match operation {
                IntegerUnaryOperation::SwapBytes => value.swap_bytes(),
                IntegerUnaryOperation::ReverseBits => value.reverse_bits(),
            })
        }};
    }
    match integer {
        crate::IntegerType::I8 => run!(i8),
        crate::IntegerType::I16 => run!(i16),
        crate::IntegerType::I32 => run!(i32),
        crate::IntegerType::I64 => run!(i64),
        crate::IntegerType::U8 => run!(u8),
        crate::IntegerType::U16 => run!(u16),
        crate::IntegerType::U32 => run!(u32),
        crate::IntegerType::U64 => run!(u64),
        crate::IntegerType::ISize | crate::IntegerType::USize => {
            unreachable!("pointer-width integers do not fold")
        }
    }
}

fn fold_integer_rotate(
    integer: crate::IntegerType,
    width: u32,
    direction: crate::IntegerRotateDirection,
    value: i128,
    count: i128,
) -> i128 {
    use crate::IntegerRotateDirection;
    let value = truncate_integer(integer, width, value);
    let count = count as u32;
    macro_rules! run {
        ($ty:ty) => {{
            let value = value as $ty;
            i128::from(match direction {
                IntegerRotateDirection::Left => value.rotate_left(count),
                IntegerRotateDirection::Right => value.rotate_right(count),
            })
        }};
    }
    match integer {
        crate::IntegerType::I8 => run!(i8),
        crate::IntegerType::I16 => run!(i16),
        crate::IntegerType::I32 => run!(i32),
        crate::IntegerType::I64 => run!(i64),
        crate::IntegerType::U8 => run!(u8),
        crate::IntegerType::U16 => run!(u16),
        crate::IntegerType::U32 => run!(u32),
        crate::IntegerType::U64 => run!(u64),
        crate::IntegerType::ISize | crate::IntegerType::USize => {
            unreachable!("pointer-width integers do not fold")
        }
    }
}

fn truncate_integer(integer: crate::IntegerType, width: u32, value: i128) -> i128 {
    match (integer.is_signed(), width) {
        (true, 8) => i128::from(value as i8),
        (true, 16) => i128::from(value as i16),
        (true, 32) => i128::from(value as i32),
        (true, 64) => i128::from(value as i64),
        (false, 8) => i128::from(value as u8),
        (false, 16) => i128::from(value as u16),
        (false, 32) => i128::from(value as u32),
        (false, 64) => i128::from(value as u64),
        _ => value,
    }
}

/// Rounds an untyped compile-time float to `float`'s precision.
pub(super) fn round_float(float: crate::FloatType, value: f64) -> f64 {
    match float {
        crate::FloatType::F32 => widen_f32(narrow_to_f32(value)),
        crate::FloatType::F64 => value,
    }
}

// Compile-time floats fold on the host, whose hardware decides the sign and
// payload of a NaN that arithmetic produces (x86-64 sets the sign bit, ARM64
// does not), and Rust leaves NaN casts unspecified. Folding fixes both, so a
// constant has the same bits whichever machine compiles it.

/// The quiet NaN an operation on non-NaN operands produces, as LLVM's own
/// constant folding does.
const CANONICAL_NAN: f64 = f64::from_bits(0x7FF8_0000_0000_0000);

/// `result`, or for a NaN result, the first NaN operand quieted, or the
/// canonical NaN when the operation itself produced one.
fn determined_nan(result: f64, operands: &[f64]) -> f64 {
    if !result.is_nan() {
        return result;
    }
    operands
        .iter()
        .find(|operand| operand.is_nan())
        .map_or(CANONICAL_NAN, |operand| {
            f64::from_bits(operand.to_bits() | 0x0008_0000_0000_0000)
        })
}

/// The `f64` that carries an `F32` value; a NaN keeps its sign, quiet bit
/// and payload exactly, so it narrows back to the same bits.
pub(super) fn widen_f32(value: f32) -> f64 {
    if !value.is_nan() {
        return f64::from(value);
    }
    let bits = value.to_bits();
    let sign = u64::from(bits >> 31) << 63;
    let mantissa = u64::from(bits & 0x007F_FFFF) << 29;
    f64::from_bits(sign | 0x7FF0_0000_0000_0000 | mantissa)
}

/// The `F32` an `f64` carries, rounding a number to `F32` precision; a NaN
/// keeps its sign, quiet bit and high payload bits.
pub(super) fn narrow_to_f32(value: f64) -> f32 {
    if !value.is_nan() {
        return value as f32;
    }
    let bits = value.to_bits();
    let sign = ((bits >> 63) as u32) << 31;
    let mut mantissa = ((bits >> 29) & 0x007F_FFFF) as u32;
    if mantissa == 0 {
        // Only low payload bits were set; stay a NaN.
        mantissa = 0x0040_0000;
    }
    f32::from_bits(sign | 0x7F80_0000 | mantissa)
}

pub(super) fn apply_float_unary(
    float: crate::FloatType,
    operation: crate::FloatUnaryOperation,
    value: f64,
) -> f64 {
    use crate::FloatUnaryOperation;
    let result = match float {
        crate::FloatType::F32 => {
            let operand = narrow_to_f32(value);
            widen_f32(match operation {
                FloatUnaryOperation::Floor => operand.floor(),
                FloatUnaryOperation::Ceil => operand.ceil(),
                FloatUnaryOperation::Trunc => operand.trunc(),
                FloatUnaryOperation::Round => operand.round(),
                FloatUnaryOperation::RoundEven => operand.round_ties_even(),
                FloatUnaryOperation::Abs => operand.abs(),
                FloatUnaryOperation::Sqrt => operand.sqrt(),
            })
        }
        crate::FloatType::F64 => match operation {
            FloatUnaryOperation::Floor => value.floor(),
            FloatUnaryOperation::Ceil => value.ceil(),
            FloatUnaryOperation::Trunc => value.trunc(),
            FloatUnaryOperation::Round => value.round(),
            FloatUnaryOperation::RoundEven => value.round_ties_even(),
            FloatUnaryOperation::Abs => value.abs(),
            FloatUnaryOperation::Sqrt => value.sqrt(),
        },
    };
    // `abs` only clears the sign bit, even of a NaN.
    if matches!(operation, FloatUnaryOperation::Abs) {
        return result;
    }
    determined_nan(result, &[value])
}

pub(super) fn apply_float_binary(
    float: crate::FloatType,
    operation: crate::FloatBinaryOperation,
    left: f64,
    right: f64,
) -> f64 {
    use crate::FloatBinaryOperation;
    let result = match float {
        crate::FloatType::F32 => {
            let (left, right) = (narrow_to_f32(left), narrow_to_f32(right));
            widen_f32(match operation {
                FloatBinaryOperation::Add => left + right,
                FloatBinaryOperation::Subtract => left - right,
                FloatBinaryOperation::Multiply => left * right,
                FloatBinaryOperation::Divide => left / right,
                FloatBinaryOperation::Remainder => left % right,
                FloatBinaryOperation::Min => left.min(right),
                FloatBinaryOperation::Max => left.max(right),
                FloatBinaryOperation::CopySign => left.copysign(right),
            })
        }
        crate::FloatType::F64 => match operation {
            FloatBinaryOperation::Add => left + right,
            FloatBinaryOperation::Subtract => left - right,
            FloatBinaryOperation::Multiply => left * right,
            FloatBinaryOperation::Divide => left / right,
            FloatBinaryOperation::Remainder => left % right,
            FloatBinaryOperation::Min => left.min(right),
            FloatBinaryOperation::Max => left.max(right),
            FloatBinaryOperation::CopySign => left.copysign(right),
        },
    };
    // `copysign` only moves a sign bit, even onto a NaN.
    if matches!(operation, FloatBinaryOperation::CopySign) {
        return result;
    }
    determined_nan(result, &[left, right])
}

pub(super) fn apply_float_ternary(
    float: crate::FloatType,
    operation: crate::FloatTernaryOperation,
    first: f64,
    second: f64,
    third: f64,
) -> f64 {
    use crate::FloatTernaryOperation;
    let FloatTernaryOperation::MulAdd = operation;
    let result = match float {
        crate::FloatType::F32 => {
            widen_f32(narrow_to_f32(first).mul_add(narrow_to_f32(second), narrow_to_f32(third)))
        }
        crate::FloatType::F64 => first.mul_add(second, third),
    };
    determined_nan(result, &[first, second, third])
}
