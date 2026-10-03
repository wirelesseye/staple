use super::*;

/// Greatest fixed point: a dependency may only remove a capability.
impl TypeChecker {
    pub(super) fn substitute_parameter_products(
        &mut self,
        ty: CheckedType,
        substitutions: &HashMap<TypeParameterId, CheckedType>,
        span: Span,
    ) -> CheckedType {
        if let Some(message) = invalid_spread_substitution(&ty, substitutions) {
            self.diagnostics.push(Diagnostic::new(span, message));
            return CheckedType::Error;
        }
        substitute_type(ty, substitutions)
    }

    pub(super) fn collect_parameter_product_capabilities(&mut self, module: &ResolvedModule) {
        let mut signatures: HashMap<TypeParameterId, Vec<(Type, bool)>> = HashMap::new();
        let mut forced_values = HashSet::new();
        for trait_ in module.traits().values() {
            forced_values.extend(trait_.parameters.iter().copied());
        }
        for (id, declaration) in module.type_declarations() {
            let builtin_value = matches!(module.builtin_type(*id), Some(BuiltinType::Buffer))
                || module.recursive_construction(*id).is_some();
            let parameters = self.declared_type_parameters(module, &declaration.type_parameters);
            if builtin_value || declaration.kind() == TypeDeclarationKind::Singleton {
                continue;
            }
            for parameter in parameters {
                let uses = signatures.entry(parameter).or_default();
                if declaration.kind() != TypeDeclarationKind::Opaque {
                    if let Some(body) = declaration.underlying() {
                        uses.push((body.clone(), false));
                    }
                    for bound in &declaration.trait_bounds {
                        uses.extend(bound.arguments.iter().cloned().map(|ty| (ty, false)));
                    }
                    for bound in &declaration.subtype_bounds {
                        if let Some(id) = module.type_parameter_for(bound.syntax.id) {
                            forced_values.insert(id);
                        }
                        uses.push((bound.supertype.clone(), false));
                    }
                }
                self.parameter_product_owners
                    .insert(parameter, declaration.name.clone());
            }
        }
        for function in module.functions() {
            for parameter in self.declared_type_parameters(module, &function.type_parameters) {
                let uses = signatures.entry(parameter).or_default();
                if let Some(target) = &function.companion_target {
                    uses.push((target.clone(), false));
                }
                // Generic `def`s always carry an annotation (the checker rejects
                // them otherwise), so the signature is never read from patterns.
                if let Some(annotation) = &function.binding_annotation {
                    uses.push((annotation.clone(), false));
                }
                if let Some(result) = &function.result_annotation {
                    uses.push((result.clone(), false));
                }
                for bound in &function.trait_bounds {
                    uses.extend(bound.arguments.iter().cloned().map(|ty| (ty, false)));
                }
                for bound in &function.subtype_bounds {
                    if let Some(id) = module.type_parameter_for(bound.syntax.id) {
                        forced_values.insert(id);
                    }
                    uses.push((Type::Named(bound.parameter.clone()), false));
                    uses.push((bound.supertype.clone(), false));
                }
                self.parameter_product_owners
                    .entry(parameter)
                    .or_insert_with(|| {
                        function
                            .name
                            .rsplit('.')
                            .next()
                            .unwrap_or(&function.name)
                            .to_owned()
                    });
            }
        }
        // Intrinsics have no function body, but their generic signatures
        // participate in the same parameter-product capability analysis.
        for source in module.program().modules() {
            for item in &source.syntax.items {
                let Item::ExternBlock(block) = item else {
                    continue;
                };
                for binding in &block.bindings {
                    for parameter in self.declared_type_parameters(module, &binding.type_parameters)
                    {
                        let uses = signatures.entry(parameter).or_default();
                        if let Some(target) = &binding.companion_target {
                            uses.push((target.clone(), false));
                        }
                        if let Some(annotation) = &binding.annotation {
                            uses.push((annotation.clone(), false));
                        }
                        for bound in &binding.trait_bounds {
                            uses.extend(bound.arguments.iter().cloned().map(|ty| (ty, false)));
                        }
                        for bound in &binding.subtype_bounds {
                            if let Some(id) = module.type_parameter_for(bound.syntax.id) {
                                forced_values.insert(id);
                            }
                            uses.push((Type::Named(bound.parameter.clone()), false));
                            uses.push((bound.supertype.clone(), false));
                        }
                        self.parameter_product_owners
                            .insert(parameter, binding.name.clone());
                    }
                }
            }
        }
        for implementation in module.trait_implementations() {
            for parameter in &implementation.parameters {
                let uses = signatures.entry(*parameter).or_default();
                uses.extend(
                    implementation
                        .arguments
                        .iter()
                        .cloned()
                        .map(|ty| (ty, false)),
                );
                for bound in &implementation.trait_bounds {
                    uses.extend(bound.arguments.iter().cloned().map(|ty| (ty, false)));
                }
                for bound in &implementation.subtype_bounds {
                    if let Some(id) = module.type_parameter_for(bound.syntax.id) {
                        forced_values.insert(id);
                    }
                    uses.push((Type::Named(bound.parameter.clone()), false));
                    uses.push((bound.supertype.clone(), false));
                }
            }
        }
        let mut capable: HashSet<_> = signatures
            .keys()
            .copied()
            .filter(|id| !forced_values.contains(id))
            .collect();
        loop {
            let mut rejected = Vec::new();
            for (&parameter, uses) in &signatures {
                if !capable.contains(&parameter) {
                    continue;
                }
                for (ty, allowed) in uses {
                    if let Some(span) =
                        capability_violation(module, ty, parameter, *allowed, &capable)
                    {
                        rejected.push((parameter, span));
                        break;
                    }
                }
            }
            if rejected.is_empty() {
                break;
            }
            for (parameter, span) in rejected {
                capable.remove(&parameter);
                self.parameter_product_violations.insert(parameter, span);
            }
        }
        self.capable_type_parameters = capable;
    }

    pub(super) fn reject_misplaced_parameter_products(&mut self, ty: &CheckedType, span: Span) {
        self.reject_parameter_product_placement(ty, false, span);
    }

    pub(super) fn reject_parameter_product_placement(
        &mut self,
        ty: &CheckedType,
        allowed: bool,
        span: Span,
    ) {
        let active = self
            .active_generic_parameters
            .iter()
            .flatten()
            .copied()
            .collect::<HashSet<_>>();
        if let Some(invalid) = misplaced(ty, allowed, &active, &self.capable_type_parameters) {
            let message = match invalid {
                CheckedType::Parameter { name, .. } => format!(
                    "`{name}` may be a parameter product; write `[{name}] -> R` to require a single value"
                ),
                _ => format!(
                    "parameter product type `{invalid}` can only be used as a function's parameter type"
                ),
            };
            self.diagnostics.push(Diagnostic::new(span, message));
        }
    }
}

fn invalid_spread_substitution(
    ty: &CheckedType,
    substitutions: &HashMap<TypeParameterId, CheckedType>,
) -> Option<String> {
    let visit = |ty| invalid_spread_substitution(ty, substitutions);
    match ty {
        CheckedType::ParameterProduct(product) => {
            let mut count = 0;
            for (index, element) in product.elements.iter().enumerate() {
                if let Some(message) = visit(&element.value_type) {
                    return Some(message);
                }
                if product.spreads.contains(&index) {
                    count += match substitute_type(element.value_type.clone(), substitutions) {
                        CheckedType::ParameterProduct(spread) => spread.elements.len(),
                        CheckedType::Product(spread) if !spread.variadic => spread.elements.len(),
                        CheckedType::Parameter { .. }
                        | CheckedType::Inferred
                        | CheckedType::Error => 1,
                        other => {
                            return Some(format!(
                                "cannot spread non-product type `{other}` in a parameter product"
                            ));
                        }
                    };
                } else {
                    count += 1;
                }
            }
            if count == 0 || count > MAX_PRODUCT_ARITY {
                return Some("a parameter product needs at least one slot and cannot exceed the product arity limit".to_owned());
            }
            None
        }
        CheckedType::Function(function) => visit(&function.parameter)
            .or_else(|| visit(&function.result))
            .or_else(|| {
                function
                    .effects
                    .resources
                    .iter()
                    .find_map(|r| visit(&r.value_type))
            }),
        CheckedType::Product(product) => product.elements.iter().find_map(|e| visit(&e.value_type)),
        CheckedType::Sum(sum) => sum.alternatives.iter().find_map(visit),
        CheckedType::Wrapper {
            arguments,
            representation,
            ..
        } => arguments
            .iter()
            .find_map(visit)
            .or_else(|| visit(representation)),
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().find_map(visit)
        }
        CheckedType::Ref(ty)
        | CheckedType::Slice(ty)
        | CheckedType::Buffer(ty)
        | CheckedType::CPointer { pointee: ty } => visit(ty),
        CheckedType::Array { element, count } => visit(element).or_else(|| visit(count)),
        _ => None,
    }
}

fn capability_violation(
    module: &ResolvedModule,
    ty: &Type,
    parameter: TypeParameterId,
    allowed: bool,
    capable: &HashSet<TypeParameterId>,
) -> Option<Span> {
    let visit = |ty: &Type, allowed| capability_violation(module, ty, parameter, allowed, capable);
    match ty {
        Type::Named(named)
            if module.type_parameter_for(named.syntax.id) == Some(parameter) && !allowed =>
        {
            Some(named.syntax.span.clone())
        }
        Type::Function(function) => visit(
            &function.parameter,
            function.mutations.is_empty() && function.moves.is_empty(),
        )
        .or_else(|| visit(&function.result, false))
        .or_else(|| {
            function
                .effects
                .resources
                .iter()
                .find_map(|resource| visit(&resource.value_type, false))
        }),
        Type::ParameterProduct(product) => product
            .elements
            .iter()
            .find_map(|element| visit(&element.ty, element.spread)),
        Type::Product(product) => product
            .elements
            .iter()
            .find_map(|element| visit(&element.ty, false)),
        Type::Sum(sum) => sum.alternatives.iter().find_map(|ty| visit(ty, false)),
        Type::Array(array) => visit(&array.element, false).or_else(|| visit(&array.count, false)),
        Type::EffectApplication(application) => visit(&application.callee, allowed).or_else(|| {
            application
                .effects
                .resources
                .iter()
                .find_map(|resource| visit(&resource.value_type, false))
        }),
        Type::Application(_) => {
            let mut arguments = Vec::new();
            let mut callee = ty;
            while let Type::Application(application) = callee {
                arguments.push(application.argument.as_ref());
                callee = &application.callee;
            }
            arguments.reverse();
            let constructor = if let Type::EffectApplication(application) = callee {
                application.callee.as_ref()
            } else {
                callee
            };
            let declaration = module
                .type_for(constructor.syntax().id)
                .and_then(|id| module.type_declarations().get(&id));
            visit(callee, false).or_else(|| {
                arguments.iter().enumerate().find_map(|(index, argument)| {
                    let accepts = declaration
                        .and_then(|d| d.type_parameters.get(index))
                        .and_then(|pattern| {
                            if let TypeParameterPattern::Binding(binding) = pattern {
                                module.type_parameter_for(binding.syntax.id)
                            } else {
                                None
                            }
                        })
                        .is_some_and(|id| capable.contains(&id));
                    visit(argument, accepts)
                })
            })
        }
        _ => None,
    }
}

fn misplaced<'a>(
    ty: &'a CheckedType,
    allowed: bool,
    active: &HashSet<TypeParameterId>,
    capable: &HashSet<TypeParameterId>,
) -> Option<&'a CheckedType> {
    let visit = |ty: &'a CheckedType, allowed| misplaced(ty, allowed, active, capable);
    match ty {
        CheckedType::ParameterProduct(_) if !allowed => Some(ty),
        CheckedType::Parameter { id, .. }
            if !allowed && active.contains(id) && capable.contains(id) =>
        {
            Some(ty)
        }
        CheckedType::Function(function) => visit(
            &function.parameter,
            function.mutations.is_empty() && function.moves.is_empty(),
        )
        .or_else(|| visit(&function.result, false))
        .or_else(|| {
            function
                .effects
                .resources
                .iter()
                .find_map(|r| visit(&r.value_type, false))
        }),
        CheckedType::Product(product) => product
            .elements
            .iter()
            .find_map(|element| visit(&element.value_type, false)),
        CheckedType::ParameterProduct(product) => {
            product
                .elements
                .iter()
                .enumerate()
                .find_map(|(index, element)| {
                    visit(&element.value_type, product.spreads.contains(&index))
                })
        }
        CheckedType::Sum(sum) => sum.alternatives.iter().find_map(|ty| visit(ty, false)),
        CheckedType::Ref(ty) | CheckedType::Slice(ty) | CheckedType::Buffer(ty) => visit(ty, false),
        CheckedType::CPointer { pointee } => visit(pointee, true),
        CheckedType::Array { element, count } => {
            visit(element, false).or_else(|| visit(count, false))
        }
        CheckedType::Wrapper {
            representation,
            arguments,
            ..
        } => arguments
            .iter()
            .find_map(|a| visit(a, true))
            .or_else(|| visit(representation, false)),
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().find_map(|a| visit(a, true))
        }
        _ => None,
    }
}

/// Explain an arity mismatch at the parameter that demands a single value.
pub(super) fn single_value_conflict<'a>(
    template: &'a CheckedType,
    actual: &CheckedType,
) -> Option<&'a CheckedType> {
    let recurse = |t, a| single_value_conflict(t, a);
    match (template, actual) {
        (
            parameter @ CheckedType::Parameter {
                parameter_product_capable: false,
                ..
            },
            CheckedType::ParameterProduct(_),
        ) => Some(parameter),
        (
            parameter @ CheckedType::Parameter {
                parameter_product_capable: false,
                ..
            },
            CheckedType::Parameter {
                parameter_product_capable: true,
                ..
            },
        ) => Some(parameter),
        (CheckedType::Function(template), CheckedType::Function(actual)) => {
            if template.parameter_style != actual.parameter_style
                && matches!(
                    template.parameter.as_ref(),
                    CheckedType::Parameter {
                        parameter_product_capable: false,
                        ..
                    }
                )
            {
                return Some(&template.parameter);
            }
            recurse(&template.parameter, &actual.parameter)
                .or_else(|| recurse(&template.result, &actual.result))
        }
        (CheckedType::Product(template), CheckedType::Product(actual)) => template
            .elements
            .iter()
            .zip(&actual.elements)
            .find_map(|(t, a)| recurse(&t.value_type, &a.value_type)),
        (
            CheckedType::Wrapper {
                arguments: template,
                ..
            },
            CheckedType::Wrapper {
                arguments: actual, ..
            },
        )
        | (
            CheckedType::Opaque {
                arguments: template,
                ..
            },
            CheckedType::Opaque {
                arguments: actual, ..
            },
        ) => template.iter().zip(actual).find_map(|(t, a)| recurse(t, a)),
        _ => None,
    }
}
