//! Template macros: overloads whose body only fills in a quotation or returns
//! a parameter, so they can be instantiated without evaluating anything else.
use super::*;

/// The parts of a template overload's curried body.
pub(super) struct TemplateParts<'a> {
    /// One pattern per curried parameter, in application order.
    pub(super) parameters: Vec<&'a Pattern>,
    /// The quotation, a type ascription of one, or the parameter name.
    pub(super) body: &'a Expression,
}

/// Splits a macro value into its curried parameters and final body when the
/// overload is a template: the body, after the parameter list, is exactly a
/// `quote`/`parse_quote` whose splices name only parameters (optionally behind
/// a type ascription), or a parameter name on its own.
pub(super) fn template_parts(value: &Expression) -> Option<TemplateParts<'_>> {
    let mut parameters = Vec::new();
    let mut body = value;
    while let Expression::Function(function) = body {
        if function.generics.is_some() || function.effect_lower_bound.is_some() {
            return None;
        }
        parameters.push(&function.pattern);
        body = &function.body;
    }
    let mut names = HashSet::new();
    for pattern in &parameters {
        if !collect_parameter_names(pattern, &mut names) {
            return None;
        }
    }
    let mut tail = body;
    while let Expression::TypeAscription(ascription) = tail {
        tail = &ascription.value;
    }
    match tail {
        Expression::Quote(quote) => {
            let tokens = quote.contents.tokens();
            let only_parameters = tokens.windows(2).all(|pair| {
                pair[0].kind != staple_syntax::TokenKind::Dollar
                    || pair[1].kind != staple_syntax::TokenKind::Identifier
                    || names.contains(&pair[1].text)
            });
            only_parameters.then_some(TemplateParts { parameters, body })
        }
        Expression::Name(name) => names
            .contains(&name.name)
            .then_some(TemplateParts { parameters, body }),
        _ => None,
    }
}

/// Collects the names a parameter pattern binds, rejecting patterns that do
/// more than name syntax values (literals, constructors, `mut` parameters).
fn collect_parameter_names(pattern: &Pattern, names: &mut HashSet<String>) -> bool {
    match pattern {
        Pattern::Binding(binding) => {
            let plain = !binding.mutable
                && binding
                    .name
                    .chars()
                    .next()
                    .is_some_and(|first| !first.is_uppercase());
            if plain {
                names.insert(binding.name.clone());
            }
            plain
        }
        Pattern::Wildcard(_) => true,
        Pattern::Product(product) => {
            !product.mutable
                && product
                    .elements
                    .iter()
                    .all(|element| collect_parameter_names(element, names))
        }
        _ => false,
    }
}

impl MacroExpander {
    /// Instantiates a template overload for `arguments` (already converted to
    /// values, one per curried parameter) by evaluating only its quotation in
    /// an environment of the bound parameters. `None` means the overload is
    /// not applicable to this call shape and the general path must run.
    pub(super) fn instantiate_template(
        &mut self,
        definition: &MacroDefinition,
        arguments: Vec<Value>,
    ) -> Option<Option<Value>> {
        let MacroKind::User(value) = &definition.kind else {
            return None;
        };
        let parts = template_parts(value)?;
        if parts.parameters.len() != arguments.len() {
            return None;
        }
        let mut environment = Environment::new();
        for (pattern, argument) in parts.parameters.iter().zip(arguments) {
            if !bind_pattern(pattern, argument, &mut environment) {
                self.diagnostics.push(Diagnostic::new(
                    definition.declaration.syntax.span.clone(),
                    "compile-time function argument does not match its parameter",
                ));
                return Some(None);
            }
        }
        let previous_context = if quote_result_type(&definition.result) {
            self.quote_context.replace(definition.result.clone())
        } else {
            self.quote_context.take()
        };
        let result = self.eval_expression(definition.key.module, parts.body, &mut environment);
        self.quote_context = previous_context;
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the single macro declared by `source` is a template overload.
    fn is_template(source: &str) -> bool {
        let module = staple_syntax::parse(source).expect("source should parse");
        let declaration = module
            .items
            .iter()
            .find_map(|item| match item {
                Item::MacroDeclaration(declaration) => Some(declaration),
                _ => None,
            })
            .expect("source declares a macro");
        let mut next_syntax_id = usize::MAX / 2;
        let declaration = with_curried_macro_value(declaration, &mut next_syntax_id);
        declaration
            .value
            .as_ref()
            .and_then(template_parts)
            .is_some()
    }

    #[test]
    fn a_quotation_over_parameters_is_a_template() {
        assert!(is_template(
            "macro twice = value: Expr => parse_quote { ($value, $value) }\n"
        ));
        assert!(is_template(
            "macro both = (left: Expr; right: Expr) => parse_quote { ($left, $right) }\n"
        ));
    }

    #[test]
    fn a_type_ascription_on_the_body_is_allowed() {
        assert!(is_template(
            "macro wrapped = value: Expr => Expr :: parse_quote { ($value) }\n"
        ));
    }

    #[test]
    fn a_parameter_on_its_own_is_a_template() {
        assert!(is_template(
            "macro @identity: Item -> Item = item => item\n"
        ));
    }

    #[test]
    fn a_let_before_the_quotation_is_not_a_template() {
        assert!(!is_template(
            "macro lets = value: Expr => {\n    let copy = value\n    parse_quote { $copy }\n}\n"
        ));
    }

    #[test]
    fn a_splice_of_a_non_parameter_is_not_a_template() {
        assert!(!is_template(
            "macro stray = value: Expr => parse_quote { ($value, $other) }\n"
        ));
    }

    #[test]
    fn a_call_in_the_body_is_not_a_template() {
        assert!(!is_template("macro called = value: Expr => helper value\n"));
        assert!(!is_template("macro unrelated = value: Expr => other\n"));
    }

    #[test]
    fn the_flow_templates_are_detected_and_the_recursive_ones_are_not() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib/std/flow.sta");
        let module = staple_syntax::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut next_syntax_id = usize::MAX / 2;
        let mut classified = Vec::new();
        for item in &module.items {
            if let Item::MacroDeclaration(declaration) = traits::plain_item(item) {
                let declaration = with_curried_macro_value(declaration, &mut next_syntax_id);
                let template = declaration
                    .value
                    .as_ref()
                    .and_then(template_parts)
                    .is_some();
                classified.push((declaration.name.clone(), template));
            }
        }
        for name in ["while", "discard", "for"] {
            assert!(
                classified.contains(&(name.to_owned(), true)),
                "{name}: {classified:?}"
            );
        }
    }
}
