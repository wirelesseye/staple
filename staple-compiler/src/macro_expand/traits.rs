//! Trait and companion dispatch for the compile-time interpreter.
use super::parse::{HeaderPattern, TypeBinding, header_pattern, instantiate_header, match_header};
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct TraitKey(pub ModuleId, pub String);

#[derive(Clone)]
pub(super) struct TraitImpl {
    pub(super) module: ModuleId,
    pub(super) declaration: staple_syntax::TraitImplementation,
    headers: Vec<HeaderPattern>,
}

#[derive(Default)]
pub(super) struct Registry {
    pub(super) declarations: HashMap<TraitKey, staple_syntax::TraitDeclaration>,
    pub(super) implementations: HashMap<TraitKey, Vec<TraitImpl>>,
    companions: Vec<HelperDefinition>,
}

pub(super) fn parameter_names(patterns: &[staple_syntax::TypeParameterPattern]) -> HashSet<String> {
    patterns
        .iter()
        .filter_map(|p| match p {
            staple_syntax::TypeParameterPattern::Binding(p) => Some(p.name.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn plain_item(mut item: &Item) -> &Item {
    while let Item::Modified(modified) = item {
        if !modified
            .modifiers
            .iter()
            .all(|m| m.namespace.is_none() && m.name == "doc")
        {
            break;
        }
        item = &modified.item;
    }
    item
}

impl Registry {
    pub(super) fn companion_member_exists(&self, name: &str) -> bool {
        self.companions
            .iter()
            .any(|helper| helper.binding.name == name)
    }

    pub(super) fn collect(program: &Program, expander: &MacroExpander) -> Self {
        let mut registry = Self::default();
        for module in program.modules() {
            for item in &module.syntax.items {
                match plain_item(item) {
                    Item::TraitDeclaration(declaration) => {
                        registry.declarations.insert(
                            TraitKey(module.id, declaration.name.clone()),
                            declaration.clone(),
                        );
                    }
                    Item::Binding(binding) if binding.companion_target.is_some() => {
                        registry.companions.push(HelperDefinition {
                            module: module.id,
                            binding: Rc::new(binding.clone()),
                        });
                    }
                    _ => {}
                }
            }
        }
        for module in program.modules() {
            for item in &module.syntax.items {
                if let Item::TraitImplementation(implementation) = plain_item(item) {
                    if implementation.negative {
                        continue;
                    }
                    let Some(key) = expander.trait_key(module.id, &implementation.trait_name)
                    else {
                        continue;
                    };
                    let parameters = parameter_names(&implementation.type_parameters);
                    let headers = implementation
                        .arguments
                        .iter()
                        .map(|ty| header_pattern(ty, &parameters))
                        .collect::<Option<Vec<_>>>()
                        .unwrap_or_default();
                    registry
                        .implementations
                        .entry(key)
                        .or_default()
                        .push(TraitImpl {
                            module: module.id,
                            declaration: implementation.clone(),
                            headers,
                        });
                }
            }
        }
        registry
    }
}

fn unknown() -> MetaType {
    MetaType::Nominal("_".into(), vec![])
}
fn is_unknown(ty: &MetaType) -> bool {
    matches!(ty, MetaType::Nominal(name, _) if name == "_")
}

/// The recoverable part of a value's type. Nominal arguments are erased.
pub(super) fn value_shape(value: &Value) -> MetaType {
    match value {
        Value::Sequence(_) => MetaType::Sequence(Box::new(unknown())),
        Value::Separated { .. } => {
            MetaType::Nominal("Separated".into(), vec![unknown(), unknown()])
        }
        Value::Nominal(name, _) => MetaType::Nominal(name.clone(), vec![]),
        Value::Integer(_, Some(integer)) => MetaType::Nominal(integer.name().into(), vec![]),
        Value::Integer(_, None) => MetaType::Nominal("{integer}".into(), vec![]),
        Value::Float(_, _) => MetaType::Nominal("{float}".into(), vec![]),
        Value::Char(_) => MetaType::Nominal("Char".into(), vec![]),
        Value::String(_) => MetaType::Nominal("String".into(), vec![]),
        Value::Product(elements, false) if elements.len() == 1 && elements[0].0.is_none() => {
            value_shape(&elements[0].1)
        }
        Value::Product(elements, slotted) => {
            let elements = elements
                .iter()
                .map(|(_, value)| value_shape(value))
                .collect();
            if *slotted {
                MetaType::SlottedProduct(elements)
            } else {
                MetaType::Product(elements)
            }
        }
        Value::Syntax(syntax) => match syntax {
            SyntaxValue::Ident(_) => MetaType::Ident(None),
            SyntaxValue::Type(_) => MetaType::Type,
            SyntaxValue::Pattern(_) => MetaType::Pattern,
            SyntaxValue::Item(_) => MetaType::Item,
            SyntaxValue::Comma(_) => MetaType::Comma,
            SyntaxValue::Equals(_) => MetaType::Equals,
            SyntaxValue::FatArrow(_) => MetaType::FatArrow,
            SyntaxValue::Call(_) => MetaType::CallExpr,
            SyntaxValue::Raw(_) => MetaType::Syntax,
            SyntaxValue::Unstructured(Expression::Char(_)) => MetaType::CharExpr,
            SyntaxValue::Unstructured(Expression::String(_)) => MetaType::StringExpr,
            SyntaxValue::Unstructured(_) => MetaType::UnstructuredExpr,
            SyntaxValue::Items(_) => MetaType::Sequence(Box::new(MetaType::Item)),
            SyntaxValue::Modifier(_) => MetaType::Modifier,
            SyntaxValue::Visibility(_) => MetaType::Visibility,
            SyntaxValue::Parameters { kind, .. } => kind.clone(),
            SyntaxValue::Delimited(value) => MetaType::Delimited(
                value.kind,
                match &value.contents {
                    DelimitedValueContents::Slotted(values) => DelimitedMetaContents::Slotted(
                        values.iter().map(|(_, v)| value_shape(v)).collect(),
                    ),
                    DelimitedValueContents::Product(values) => DelimitedMetaContents::Product(
                        values.iter().map(|(_, v)| value_shape(v)).collect(),
                    ),
                    DelimitedValueContents::Sequence(_) => {
                        DelimitedMetaContents::Sequence(Box::new(unknown()))
                    }
                    DelimitedValueContents::Separated { .. } => DelimitedMetaContents::Separated {
                        element: Box::new(unknown()),
                        separator: Box::new(unknown()),
                    },
                },
            ),
        },
        _ => unknown(),
    }
}

fn shapes_agree(left: &MetaType, right: &MetaType) -> bool {
    if is_unknown(left) || is_unknown(right) {
        return true;
    }
    match (left, right) {
        (MetaType::Nominal(left, a), MetaType::Nominal(right, b)) => {
            let integer = |name: &str| {
                matches!(
                    name,
                    "{integer}"
                        | "I8"
                        | "I16"
                        | "I32"
                        | "I64"
                        | "I128"
                        | "ISize"
                        | "U8"
                        | "U16"
                        | "U32"
                        | "U64"
                        | "U128"
                        | "USize"
                )
            };
            let float = |name: &str| matches!(name, "{float}" | "F32" | "F64");
            (left == right
                && (a.is_empty()
                    || b.is_empty()
                    || a.len() == b.len() && a.iter().zip(b).all(|(a, b)| shapes_agree(a, b))))
                || ((left == "{integer}" || right == "{integer}")
                    && integer(left)
                    && integer(right))
                || ((left == "{float}" || right == "{float}") && float(left) && float(right))
        }
        (MetaType::Sequence(a), MetaType::Sequence(b))
        | (MetaType::Optional(a), MetaType::Optional(b)) => shapes_agree(a, b),
        (MetaType::Product(a), MetaType::Product(b))
        | (MetaType::SlottedProduct(a), MetaType::SlottedProduct(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| shapes_agree(a, b))
        }
        _ => left == right,
    }
}

fn match_shape(
    pattern: &HeaderPattern,
    shape: &MetaType,
    env: &mut HashMap<String, TypeBinding>,
) -> bool {
    if is_unknown(shape) {
        return true;
    }
    match (pattern, shape) {
        (HeaderPattern::Parameter(name), _) => {
            // Erased shapes may refine an existing binding, never erase it.
            match env.get(name) {
                Some(TypeBinding::Meta(existing)) if !is_unknown(existing) => {
                    shapes_agree(existing, shape)
                }
                _ => {
                    env.insert(name.clone(), TypeBinding::Meta(shape.clone()));
                    true
                }
            }
        }
        (HeaderPattern::Sequence(inner), MetaType::Sequence(element)) => {
            match_shape(inner, element, env)
        }
        (HeaderPattern::Nominal(name, _), MetaType::Nominal(actual, args)) if args.is_empty() => {
            name == actual
                || (actual == "{integer}"
                    && matches!(
                        name.as_str(),
                        "I8" | "I16"
                            | "I32"
                            | "I64"
                            | "I128"
                            | "ISize"
                            | "U8"
                            | "U16"
                            | "U32"
                            | "U64"
                            | "U128"
                            | "USize"
                    ))
                || (actual == "{float}" && matches!(name.as_str(), "F32" | "F64"))
        }
        (HeaderPattern::Nominal(name, patterns), MetaType::Nominal(actual, args)) => {
            name == actual
                && patterns.len() == args.len()
                && patterns
                    .iter()
                    .zip(args)
                    .all(|(p, s)| match_shape(p, s, env))
        }
        (HeaderPattern::Product(patterns, _), MetaType::Product(elements))
        | (HeaderPattern::Slotted(patterns, _), MetaType::SlottedProduct(elements)) => {
            patterns.len() == elements.len()
                && patterns
                    .iter()
                    .zip(elements)
                    .all(|(p, s)| match_shape(p, s, env))
        }
        _ => match_header(pattern, shape, env),
    }
}

fn mentions(ty: &Type, parameter: &str) -> bool {
    match ty {
        Type::Named(name) => name.namespace.is_none() && name.name == parameter,
        Type::Application(application) => {
            mentions(&application.callee, parameter) || mentions(&application.argument, parameter)
        }
        Type::Product(product) => product
            .elements
            .iter()
            .any(|element| mentions(&element.ty, parameter)),
        Type::Function(function) => {
            mentions(&function.parameter, parameter) || mentions(&function.result, parameter)
        }
        Type::Forall(forall) => mentions(&forall.body, parameter),
        Type::Sum(sum) => sum.alternatives.iter().any(|ty| mentions(ty, parameter)),
        _ => false,
    }
}

fn function_type(mut ty: &Type) -> Option<&staple_syntax::FunctionType> {
    while let Type::Forall(forall) = ty {
        ty = &forall.body;
    }
    if let Type::Function(function) = ty {
        Some(function)
    } else {
        None
    }
}

impl MacroExpander {
    pub(super) fn trait_key(
        &self,
        module: ModuleId,
        named: &staple_syntax::NamedType,
    ) -> Option<TraitKey> {
        let helper = match &named.namespace {
            Some(namespace) => self.scopes[module.0]
                .namespaces
                .get(namespace)
                .and_then(|m| self.scopes[m.0].helpers.get(&named.name))
                .cloned(),
            None => self.lookup_helper(module, &named.name),
        }?;
        Some(TraitKey(helper.module, helper.binding.name.clone()))
    }

    pub(super) fn trait_member(
        &self,
        module: ModuleId,
        expression: &Expression,
        local: impl Fn(&str) -> bool,
    ) -> Option<(TraitKey, String)> {
        let (head, member) = match expression {
            Expression::Access(access) => {
                let Accessor::Name(member) = &access.accessor else {
                    return None;
                };
                (Some(access.value.as_ref()), member.clone())
            }
            Expression::Name(name)
                if !local(&name.name) && self.lookup_helper(module, &name.name).is_none() =>
            {
                (None, name.name.clone())
            }
            _ => return None,
        };
        if let Some(head) = head {
            let helper = match head {
                Expression::Name(name) if !local(&name.name) => self.lookup_helper(
                    name.syntax
                        .definition_module()
                        .map(ModuleId)
                        .unwrap_or(module),
                    &name.name,
                ),
                Expression::Access(access) => {
                    let (namespace, name, context) = qualified_macro_access_path(access)?;
                    self.scopes[context.map(ModuleId).unwrap_or(module).0]
                        .namespaces
                        .get(&namespace)
                        .and_then(|m| self.scopes[m.0].helpers.get(&name))
                        .cloned()
                }
                _ => None,
            }?;
            let key = TraitKey(helper.module, helper.binding.name.clone());
            return self
                .trait_registry
                .declarations
                .get(&key)?
                .members
                .iter()
                .any(|m| m.name == member)
                .then_some((key, member));
        }
        let mut matches = self.scopes[module.0].helpers.values().filter_map(|helper| {
            let key = TraitKey(helper.module, helper.binding.name.clone());
            self.trait_registry
                .declarations
                .get(&key)?
                .members
                .iter()
                .any(|m| m.name == member)
                .then_some((key, member.clone()))
        });
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    fn bound_satisfied(
        &self,
        module: ModuleId,
        bound: &staple_syntax::TraitBound,
        env: &HashMap<String, TypeBinding>,
        depth: usize,
    ) -> bool {
        if matches!(bound.trait_name.name.as_str(), "Copy" | "Sized") {
            return true;
        }
        if depth > 64 {
            return false;
        }
        let Some(key) = self.trait_key(module, &bound.trait_name) else {
            return false;
        };
        let targets: Vec<_> = bound
            .arguments
            .iter()
            .map(|ty| parse::parse_target_meta(ty, env))
            .collect();
        if targets
            .iter()
            .any(|target| target.as_ref().is_none_or(is_unknown))
        {
            return true;
        }
        self.trait_registry
            .implementations
            .get(&key)
            .is_some_and(|impls| {
                impls.iter().any(|implementation| {
                    let mut bindings = HashMap::new();
                    implementation
                        .headers
                        .iter()
                        .zip(&targets)
                        .all(|(header, target)| {
                            match_shape(header, target.as_ref().unwrap(), &mut bindings)
                        })
                        && implementation.declaration.trait_bounds.iter().all(|b| {
                            self.bound_satisfied(implementation.module, b, &bindings, depth + 1)
                        })
                })
            })
    }

    pub(super) fn apply_trait_member(
        &mut self,
        key: TraitKey,
        member: String,
        argument: Value,
        target: Option<MetaType>,
        explicit: Vec<Type>,
        mut pending: Vec<Value>,
        span: Span,
    ) -> Option<Value> {
        let signature = self
            .trait_registry
            .declarations
            .get(&key)?
            .members
            .iter()
            .find(|m| m.name == member)?;
        let slots = match function_type(&signature.annotation)?.parameter.as_ref() {
            Type::Product(product) if product.slotted => product.elements.len(),
            _ => 1,
        };
        if slots > 1
            && !(pending.is_empty()
                && matches!(&argument,Value::Product(fields,true) if fields.len() == slots))
        {
            pending.push(argument);
            if pending.len() < slots {
                return Some(Value::TraitMember(key, member, explicit, pending));
            }
            let argument = Value::Product(
                pending.into_iter().map(|value| (None, value)).collect(),
                true,
            );
            return self.dispatch_trait(key, member, argument, target, explicit, span);
        }
        self.dispatch_trait(key, member, argument, target, explicit, span)
    }

    pub(super) fn dispatch_trait(
        &mut self,
        key: TraitKey,
        member: String,
        argument: Value,
        target: Option<MetaType>,
        explicit: Vec<Type>,
        span: Span,
    ) -> Option<Value> {
        let declaration = self.trait_registry.declarations.get(&key)?.clone();
        let signature = declaration
            .members
            .iter()
            .find(|m| m.name == member)?
            .clone();
        let trait_parameters = parameter_names(declaration.explicit_type_parameters());
        let mut parameters = trait_parameters.clone();
        parameters.extend(
            declaration
                .associated_types
                .iter()
                .map(|associated| associated.name.clone()),
        );
        if let Type::Forall(forall) = &signature.annotation {
            parameters.extend(parameter_names(&forall.generics.parameters));
        }
        let function = function_type(&signature.annotation)?;
        let mut trait_env = HashMap::new();
        let shape = value_shape(&argument);
        let parameter_type = match function.parameter.as_ref() {
            Type::Product(product)
                if product.slotted
                    && product.elements.len() > 1
                    && !matches!(&argument, Value::Product(_, true)) =>
            {
                &product.elements[0].ty
            }
            ty => ty,
        };
        if let Some(pattern) = header_pattern(parameter_type, &parameters) {
            if !match_shape(&pattern, &shape, &mut trait_env) {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("receiver does not match `{}.{member}`", key.1),
                ));
                return None;
            }
        }
        if let Some(target) = target {
            if let Some(pattern) = header_pattern(&function.result, &parameters) {
                // Only constrain result occurrences of trait parameters.
                if parameters.iter().any(|p| mentions(&function.result, p))
                    && !match_shape(&pattern, &target, &mut trait_env)
                {
                    self.diagnostics.push(Diagnostic::new(
                        span,
                        format!(
                            "receiver and result types disagree for `{}.{member}`",
                            key.1
                        ),
                    ));
                    return None;
                }
            }
        }
        let member_has_generics = matches!(signature.annotation, Type::Forall(_));
        for (parameter, ty) in
            declaration
                .explicit_type_parameters()
                .iter()
                .zip(if member_has_generics {
                    &[]
                } else {
                    explicit.as_slice()
                })
        {
            if let staple_syntax::TypeParameterPattern::Binding(p) = parameter {
                if let Some(meta) = self.resolve_type(ty) {
                    trait_env.insert(p.name.clone(), TypeBinding::Meta(meta));
                }
            }
        }
        for parameter in &trait_parameters {
            if !trait_env.contains_key(parameter)
                && !mentions(&function.parameter, parameter)
                && mentions(&function.result, parameter)
            {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("cannot determine `{parameter}` for `{}.{member}`", key.1),
                ));
                return None;
            }
        }
        let mut candidates = Vec::new();
        for implementation in self
            .trait_registry
            .implementations
            .get(&key)
            .into_iter()
            .flatten()
        {
            if implementation.headers.len() != implementation.declaration.arguments.len() {
                continue;
            }
            let mut env = HashMap::new();
            let matches = implementation
                .headers
                .iter()
                .zip(declaration.explicit_type_parameters())
                .all(|(header, p)| {
                    let staple_syntax::TypeParameterPattern::Binding(p) = p else {
                        return true;
                    };
                    match trait_env.get(&p.name) {
                        Some(TypeBinding::Meta(meta)) => match_shape(header, meta, &mut env),
                        _ => true,
                    }
                });
            for parameter in parameter_names(&implementation.declaration.type_parameters) {
                env.entry(parameter)
                    .or_insert_with(|| TypeBinding::Meta(unknown()));
            }
            if matches
                && implementation
                    .declaration
                    .trait_bounds
                    .iter()
                    .all(|b| self.bound_satisfied(implementation.module, b, &env, 0))
            {
                candidates.push((implementation.clone(), env));
            }
        }
        if candidates.len() != 1 {
            let description = if candidates.is_empty() {
                format!(
                    "`{}` does not implement `{}`",
                    format_meta_type(&shape),
                    key.1
                )
            } else if trait_env.is_empty() {
                format!("cannot determine target type for `{}.{member}`", key.1)
            } else {
                format!(
                    "ambiguous `{}` implementations: {}",
                    key.1,
                    candidates
                        .iter()
                        .map(|(i, _)| i
                            .declaration
                            .arguments
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" "))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            self.diagnostics.push(Diagnostic::new(span, description));
            return None;
        }
        let (implementation, mut env) = candidates.pop().unwrap();
        for associated in &implementation.declaration.associated_types {
            let target = self
                .resolve_type_in(&associated.value, &env, 0)
                .unwrap_or_else(unknown);
            trait_env.insert(associated.name.clone(), TypeBinding::Meta(target));
        }
        if let Type::Forall(forall) = &signature.annotation {
            for (parameter, argument) in forall.generics.parameters.iter().zip(&explicit) {
                if let staple_syntax::TypeParameterPattern::Binding(parameter) = parameter {
                    if let Some(target) = self.resolve_type(argument) {
                        trait_env.insert(parameter.name.clone(), TypeBinding::Meta(target));
                    }
                }
            }
        }
        let result_target = self.resolve_type_in(&function.result, &trait_env, 0);
        let (module, body) = if let Some(body) = implementation
            .declaration
            .members
            .iter()
            .find(|m| m.name == member)
        {
            (implementation.module, body.value.clone())
        } else {
            for (header, parameter) in implementation
                .headers
                .iter()
                .zip(declaration.explicit_type_parameters())
            {
                if let staple_syntax::TypeParameterPattern::Binding(p) = parameter {
                    if let Some(meta) = instantiate_header(header, &env) {
                        trait_env.insert(p.name.clone(), TypeBinding::Meta(meta));
                    }
                }
            }
            env.extend(trait_env);
            (key.0, signature.default?)
        };
        let place = self.argument_place.take();
        let previous = std::mem::replace(&mut self.type_env, Rc::new(env));
        let callable = self.eval_expression(module, &body, &mut Environment::new());
        self.type_env = previous;
        let mut callable = callable?;
        if member_has_generics {
            self.bind_type_arguments(&mut callable, &explicit);
        }
        if let Value::Function {
            result_target: target,
            ..
        } = &mut callable
        {
            *target = result_target;
        }
        self.argument_place = place;
        self.apply_value(callable, argument, span)
    }

    pub(super) fn callable_takes_mut(&self, value: &Value) -> bool {
        match value {
            Value::TraitMember(key, member, _, _) => self
                .trait_registry
                .declarations
                .get(key)
                .and_then(|declaration| declaration.members.iter().find(|m| &m.name == member))
                .and_then(|member| function_type(&member.annotation))
                .is_some_and(|function| !function.mutations().is_empty()),
            Value::TypeApplied(value, _) => self.callable_takes_mut(value),
            _ => callee_takes_mut(value),
        }
    }

    pub(super) fn resolve_type(&self, ty: &Type) -> Option<MetaType> {
        self.resolve_type_in(ty, &self.type_env, 0)
    }

    fn resolve_type_in(
        &self,
        ty: &Type,
        env: &HashMap<String, TypeBinding>,
        depth: usize,
    ) -> Option<MetaType> {
        if depth > 64 {
            return None;
        }
        if let Type::Application(application) = ty {
            if let Type::Named(projection) = application.callee.as_ref() {
                if let Some(trait_name) = &projection.namespace {
                    let target = self.resolve_type_in(&application.argument, env, depth + 1)?;
                    let mut matches = Vec::new();
                    for (key, impls) in &self.trait_registry.implementations {
                        if &key.1 != trait_name {
                            continue;
                        }
                        for implementation in impls {
                            let mut bindings = HashMap::new();
                            if implementation
                                .headers
                                .first()
                                .is_some_and(|header| match_shape(header, &target, &mut bindings))
                            {
                                for parameter in
                                    parameter_names(&implementation.declaration.type_parameters)
                                {
                                    bindings
                                        .entry(parameter)
                                        .or_insert_with(|| TypeBinding::Meta(unknown()));
                                }
                                if let Some(binding) = implementation
                                    .declaration
                                    .associated_types
                                    .iter()
                                    .find(|b| b.name == projection.name)
                                {
                                    matches.push(self.resolve_type_in(
                                        &binding.value,
                                        &bindings,
                                        depth + 1,
                                    ));
                                }
                            }
                        }
                    }
                    return if matches.len() == 1 {
                        matches.pop().flatten()
                    } else {
                        None
                    };
                }
            }
        }
        if let Some(meta) = parse::parse_target_meta(ty, env) {
            return (!is_unknown(&meta)).then_some(meta);
        }
        // Projections can also occur inside a collection or product target.
        match ty {
            Type::Application(_) => {
                let mut arguments = Vec::new();
                let mut head = ty;
                while let Type::Application(application) = head {
                    arguments.push(self.resolve_type_in(&application.argument, env, depth + 1)?);
                    head = &application.callee;
                }
                arguments.reverse();
                let Type::Named(name) = head else {
                    return None;
                };
                if name.namespace.is_some() {
                    return None;
                }
                Some(match (name.name.as_str(), arguments.as_slice()) {
                    ("Sequence", [element]) => MetaType::Sequence(Box::new(element.clone())),
                    ("Option", [element]) => MetaType::Optional(Box::new(element.clone())),
                    (name, _) => MetaType::Nominal(name.to_owned(), arguments),
                })
            }
            Type::Product(product) => {
                let mut elements = product
                    .elements
                    .iter()
                    .map(|element| self.resolve_type_in(&element.ty, env, depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                Some(if elements.len() == 1 && !product.slotted {
                    elements.remove(0)
                } else if product.slotted {
                    MetaType::SlottedProduct(elements)
                } else {
                    MetaType::Product(elements)
                })
            }
            _ => None,
        }
    }

    pub(super) fn resolve_method(
        &mut self,
        module: ModuleId,
        name: &str,
        receiver: &Value,
        span: Span,
    ) -> Option<Value> {
        let shape = value_shape(receiver);
        let mut levels: [Vec<(HelperDefinition, HashMap<String, TypeBinding>)>; 2] =
            Default::default();
        for helper in &self.trait_registry.companions {
            if helper.binding.name != name {
                continue;
            }
            let generics = helper.binding.companion_generics.as_ref();
            let parameters = generics
                .map(|g| parameter_names(&g.parameters))
                .unwrap_or_default();
            let Some(pattern) =
                header_pattern(helper.binding.companion_target.as_ref()?, &parameters)
            else {
                continue;
            };
            let mut env = HashMap::new();
            if match_shape(&pattern, &shape, &mut env)
                && generics.is_none_or(|g| {
                    g.trait_bounds
                        .iter()
                        .all(|b| self.bound_satisfied(helper.module, b, &env, 0))
                })
            {
                let level = usize::from(matches!(pattern, HeaderPattern::Parameter(_)));
                levels[level].push((helper.clone(), env));
            }
        }
        for candidates in levels {
            if candidates.len() > 1 {
                self.diagnostics.push(Diagnostic::new(
                    span,
                    format!("ambiguous compile-time companion member `{name}`"),
                ));
                return None;
            }
            if let Some((helper, env)) = candidates.into_iter().next() {
                let previous = std::mem::replace(&mut self.type_env, Rc::new(env));
                let value = self.eval_expression(
                    helper.module,
                    helper.binding.value.as_ref()?,
                    &mut Environment::new(),
                );
                self.type_env = previous;
                return value;
            }
        }
        if let Some(helper) = self.lookup_helper(module, name) {
            return self.eval_helper_binding(helper, span);
        }
        let expression = Expression::Name(staple_syntax::NameExpression {
            syntax: Syntax::compiler(),
            name: name.into(),
        });
        if let Some((key, member)) = self.trait_member(module, &expression, |_| false) {
            return Some(Value::TraitMember(key, member, vec![], vec![]));
        }
        self.diagnostics.push(Diagnostic::new(
            span,
            format!("compile-time member `{name}` is not available"),
        ));
        None
    }
}

impl MacroExpander {
    pub(super) fn bind_type_arguments(&self, value: &mut Value, arguments: &[Type]) {
        if let Value::Function {
            function, type_env, ..
        } = value
        {
            if let Some(generics) = &function.generics {
                let env = Rc::make_mut(type_env);
                for (parameter, argument) in generics.parameters.iter().zip(arguments) {
                    if let staple_syntax::TypeParameterPattern::Binding(p) = parameter {
                        if let Some(meta) = self.resolve_type(argument) {
                            env.insert(p.name.clone(), TypeBinding::Meta(meta));
                        }
                    }
                }
            }
        }
    }

    pub(super) fn bind_function_types(
        &self,
        function: &staple_syntax::FunctionExpression,
        argument: &Value,
        type_env: &mut TypeEnv,
    ) {
        let mut params = function
            .generics
            .as_ref()
            .map(|g| parameter_names(&g.parameters))
            .unwrap_or_default();
        params.extend(type_env.keys().cloned());
        let env = Rc::make_mut(type_env);
        // Bind the result before argument shapes; explicit bindings win.
        if let Expression::TypeAscription(ascription) = function.body.as_ref() {
            if let (Some(pattern), Some(target)) =
                (header_pattern(&ascription.ty, &params), &self.call_target)
            {
                match_shape(&pattern, target, env);
            }
        }
        fn bind(
            pattern: &Pattern,
            value: &Value,
            params: &HashSet<String>,
            env: &mut HashMap<String, TypeBinding>,
        ) {
            match pattern {
                Pattern::Binding(binding) => {
                    if let Some(pattern) = header_pattern(&binding.ty, params) {
                        match_shape(&pattern, &value_shape(value), env);
                    }
                }
                Pattern::Product(product) => {
                    if let Value::Product(elements, _) = value {
                        for (p, (_, v)) in product.elements.iter().zip(elements) {
                            bind(p, v, params, env);
                        }
                    } else if let Some(first) = product.elements.first() {
                        bind(first, value, params, env);
                    }
                }
                _ => {}
            }
        }
        bind(&function.pattern, argument, &params, env);
        for parameter in params {
            env.entry(parameter)
                .or_insert_with(|| TypeBinding::Meta(unknown()));
        }
    }
}
