use super::*;

impl TypeChecker {
    pub(super) fn substitute_product_spreads(
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
}

fn invalid_spread_substitution(
    ty: &CheckedType,
    substitutions: &HashMap<TypeParameterId, CheckedType>,
) -> Option<String> {
    let visit = |ty| invalid_spread_substitution(ty, substitutions);
    match ty {
        CheckedType::Function(function) => visit(&function.parameter)
            .or_else(|| visit(&function.result))
            .or_else(|| {
                function
                    .effects
                    .resources
                    .iter()
                    .find_map(|resource| visit(&resource.value_type))
            }),
        CheckedType::Product(product) => {
            let mut count = 0usize;
            for (index, run) in product.runs.iter().enumerate() {
                if let Some(message) = visit(&run.element.value_type) {
                    return Some(message);
                }
                if let RunCount::Symbolic(count) = &run.count
                    && let Some(message) = visit(count)
                {
                    return Some(message);
                }
                if product.spreads.contains(&index) {
                    count = count.saturating_add(
                        match substitute_type(run.element.value_type.clone(), substitutions) {
                            CheckedType::Product(spread) if !spread.variadic => {
                                spread.position_count()
                            }
                            CheckedType::Parameter { .. }
                            | CheckedType::Inferred
                            | CheckedType::Error => 1,
                            other => {
                                return Some(format!("cannot spread non-product type `{other}`"));
                            }
                        },
                    );
                } else {
                    count = count.saturating_add(run.positions().unwrap_or(1));
                }
            }
            if count > MAX_PRODUCT_ARITY || (product.slotted && count == 0) {
                return Some("a slotted product needs at least one slot and cannot exceed the product arity limit".into());
            }
            None
        }
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
        CheckedType::RawMutPtr { pointee: ty } | CheckedType::RawPtr { pointee: ty } => visit(ty),
        _ => None,
    }
}
