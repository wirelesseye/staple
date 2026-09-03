//! Reconstructs source text for the entry module after macro expansion, for
//! `staple expand`.
//!
//! Macro expansion rewrites the AST in place but only regenerates a node's
//! token stream when the whole node is replaced wholesale (a top-level
//! item-position macro). For an expansion nested inside otherwise-untouched
//! syntax — a macro call in a function body, a `parse_quote` splice — the
//! enclosing node keeps its pre-expansion tokens, so [`Syntax::text`] alone is
//! stale there.
//!
//! This module walks the expanded AST and splices each faithful subtree's text
//! back into its parent:
//!
//!   * A subtree with no generated syntax anywhere inside it is emitted
//!     verbatim from its own tokens ([`Syntax::text`]).
//!   * Otherwise the node is rebuilt: its parent's tokens are emitted with the
//!     token span of every changed child replaced by that child's rendered
//!     text, recursively.
//!
//! Generated ([`Syntax::is_generated`]) nodes are emitted from their expanded
//! fields, including declaration types and patterns. Inline modules are read
//! from the expanded flattened module table rather than their parsed snapshot.
//! The completed source is parsed again before it is returned.

use crate::{
    Accessor, BlockExpression, Expression, Item, LogicalOperator, Pattern, Program, Submodule,
    Syntax, SyntaxToken, TokenKind, Visibility,
};
use crate::{ParseError, formatter::format_token_stream, lex, parse};

/// Renders the entry module of `program` (already macro-expanded) as source.
pub fn render_expanded_module(program: &Program) -> Result<String, ParseError> {
    let module = &program.module(program.entry()).syntax;
    let mut output = String::new();
    render_items(program, &module.items, 0, &mut output);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    let output = format_token_stream(&lex(&output));
    parse(&output)?;
    Ok(output)
}

fn render_items(program: &Program, items: &[Item], indent: usize, output: &mut String) {
    for item in items {
        if let Item::Submodule(submodule) = item {
            output.push_str(&indent_lines(&submodule_header(submodule), indent));
            output.push('\n');
            let items = program
                .child_module(submodule.syntax.id)
                .map(|module| program.module(module).syntax.items.as_slice())
                .unwrap_or(&submodule.module.items);
            render_items(program, items, indent + 1, output);
            output.push_str(&indent_lines("}", indent));
            output.push_str("\n\n");
            continue;
        }
        let text = render_item(program, item);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        output.push_str(&indent_lines(text, indent));
        output.push_str(if indent == 0 { "\n\n" } else { "\n" });
    }
}

fn indent_lines(text: &str, indent: usize) -> String {
    if indent == 0 {
        return text.to_owned();
    }
    let pad = "    ".repeat(indent);
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{pad}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn submodule_header(submodule: &Submodule) -> String {
    if submodule.companion {
        let target = submodule
            .companion_target
            .as_ref()
            .map(|ty| ty.syntax().text().trim().to_owned())
            .unwrap_or_else(|| submodule.name.clone());
        format!("companion {target} {{")
    } else {
        let visibility = match submodule.visibility {
            Visibility::Public => "pub ",
            Visibility::Package => "pub(package) ",
            Visibility::Private => "",
        };
        format!("{visibility}mod {} {{", submodule.name)
    }
}

// --- items --------------------------------------------------------------------

fn render_item(program: &Program, item: &Item) -> String {
    if !item_has_generated(item) {
        return item.syntax().text().trim().to_owned();
    }
    match item {
        Item::Expression(expression) => render_expr(program, expression),
        Item::Binding(binding) if binding.syntax.is_generated() => render_binding(program, binding),
        Item::Binding(binding) => match &binding.value {
            Some(value) => prefixed(program, &binding.syntax, TokenKind::Equals, value),
            None => binding.syntax.text().trim().to_owned(),
        },
        Item::PatternBinding(binding) if binding.syntax.is_generated() => format!(
            "let {}{} = {}",
            render_pattern(&binding.pattern),
            if binding.kind == crate::PatternBindingKind::Propagating {
                " ?"
            } else {
                ""
            },
            render_expr(program, &binding.value),
        ),
        Item::PatternBinding(binding) => {
            prefixed(program, &binding.syntax, TokenKind::Equals, &binding.value)
        }
        Item::Assignment(assignment) => format!(
            "{} = {}",
            render_expr(program, &assignment.target),
            render_expr(program, &assignment.value),
        ),
        Item::Return(item) => prefixed(program, &item.syntax, TokenKind::Return, &item.value),
        Item::Break(item) => match &item.value {
            Some(value) => prefixed(program, &item.syntax, TokenKind::Break, value),
            None => item.syntax.text().trim().to_owned(),
        },
        Item::TraitImplementation(implementation) => {
            let children = implementation
                .members
                .iter()
                .map(|member| &member.value)
                .collect::<Vec<_>>();
            splice(program, &implementation.syntax, &children)
        }
        Item::TraitDeclaration(declaration) => {
            let children = declaration
                .members
                .iter()
                .filter_map(|member| member.default.as_ref())
                .collect::<Vec<_>>();
            splice(program, &declaration.syntax, &children)
        }
        Item::TypeDeclaration(declaration) => render_type_declaration(declaration),
        Item::Modified(modified) => render_item(program, &modified.item),
        Item::VisibilitySplice(splice) => render_item(program, &splice.item),
        _ => item.syntax().text().trim().to_owned(),
    }
}

fn visibility_prefix(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "pub ",
        Visibility::Package => "pub(package) ",
        Visibility::Private => "",
    }
}

fn render_binding(program: &Program, binding: &crate::Binding) -> String {
    let mut out = String::new();
    out.push_str(visibility_prefix(binding.visibility));
    if !binding.external {
        out.push_str(binding.keyword());
        out.push(' ');
        if binding.mutable {
            out.push_str("mut ");
        }
        if binding.signal {
            out.push_str("signal ");
        }
    }
    out.push_str(&binding.name);
    if !binding.type_parameters.is_empty() {
        out.push('<');
        out.push_str(
            &binding
                .type_parameters
                .iter()
                .map(render_type_parameter)
                .collect::<Vec<_>>()
                .join(", "),
        );
        let constraints = render_constraints(
            &binding.type_parameters,
            &binding.trait_bounds,
            &binding.subtype_bounds,
        );
        if !constraints.is_empty() {
            out.push_str(" where ");
            out.push_str(&constraints);
        }
        out.push('>');
    }
    if let Some(annotation) = &binding.annotation {
        out.push_str(": ");
        out.push_str(&annotation.to_string());
    }
    if let Some(value) = &binding.value {
        out.push_str(" = ");
        out.push_str(&render_expr(program, value));
    }
    out
}

fn render_type_declaration(declaration: &crate::TypeDeclaration) -> String {
    let mut out = String::new();
    if declaration.representation_visibility == Visibility::Private {
        out.push_str(visibility_prefix(declaration.visibility));
    } else {
        out.push_str(match declaration.representation_visibility {
            Visibility::Public => "pub(repr) ",
            Visibility::Package => "pub(repr(package)) ",
            Visibility::Private => "",
        });
    }
    out.push_str("type ");
    if declaration.kind == crate::TypeDeclarationKind::Alias {
        out.push_str("alias ");
    }
    out.push_str(&declaration.name);
    for parameter in &declaration.type_parameters {
        out.push(' ');
        if let Some(default) = declaration
            .default_bounds
            .iter()
            .find(|bound| bound.parameter.name == *parameter.names().first().unwrap_or(&""))
        {
            out.push('(');
            out.push_str(&render_type_parameter(parameter));
            out.push_str(" = ");
            out.push_str(&default.default.to_string());
            out.push(')');
        } else {
            out.push_str(&render_type_parameter(parameter));
        }
    }
    let constraints = render_constraints(
        &declaration.type_parameters,
        &declaration.trait_bounds,
        &declaration.subtype_bounds,
    );
    if !constraints.is_empty() {
        out.push_str(" where ");
        out.push_str(&constraints);
    }
    match declaration.kind {
        crate::TypeDeclarationKind::Alias | crate::TypeDeclarationKind::Distinct => {
            if let Some(underlying) = &declaration.underlying {
                out.push_str(" = ");
                out.push_str(&underlying.to_string());
            }
        }
        crate::TypeDeclarationKind::Opaque => out.push_str(" = opaque"),
        crate::TypeDeclarationKind::Singleton => {}
    }
    out
}

fn render_type_parameter(parameter: &crate::TypeParameterPattern) -> String {
    match parameter {
        crate::TypeParameterPattern::Binding(binding) => binding.name.clone(),
        crate::TypeParameterPattern::Effect(binding) => format!("effect {}", binding.name),
        crate::TypeParameterPattern::Product(product) => format!(
            "({})",
            product
                .elements
                .iter()
                .map(render_type_parameter)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        crate::TypeParameterPattern::Splice(splice) => format!("${}...", splice.name),
    }
}

fn render_constraints(
    parameters: &[crate::TypeParameterPattern],
    traits: &[crate::TraitBound],
    subtypes: &[crate::SubtypeBound],
) -> String {
    let mut entries = parameters
        .iter()
        .filter_map(|parameter| match parameter {
            crate::TypeParameterPattern::Binding(binding) if !binding.sized => {
                Some(format!("?Sized {}", binding.name))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    entries.extend(traits.iter().map(|bound| {
        let name = match &bound.trait_name.namespace {
            Some(namespace) => format!("{namespace}.{}", bound.trait_name.name),
            None => bound.trait_name.name.clone(),
        };
        format!(
            "{} {}",
            name,
            bound
                .arguments
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        )
    }));
    entries.extend(
        subtypes
            .iter()
            .map(|bound| format!("{} <: {}", bound.parameter.name, bound.supertype)),
    );
    entries.join(", ")
}

fn render_pattern(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Binding(binding) => {
            let mut out = String::new();
            if binding.mutable {
                out.push_str("mut ");
            }
            if binding.moved {
                out.push_str("move ");
            }
            out.push_str(&binding.name);
            if !matches!(binding.ty, crate::Type::Inferred(_)) {
                out.push_str(": ");
                out.push_str(&binding.ty.to_string());
            }
            out
        }
        Pattern::At(at) => format!(
            "{} @ {}",
            render_pattern(&Pattern::Binding((*at.binding).clone())),
            render_pattern(&at.pattern)
        ),
        Pattern::Wildcard(wildcard) => {
            if matches!(wildcard.ty, crate::Type::Inferred(_)) {
                "_".to_owned()
            } else {
                format!("_: {}", wildcard.ty)
            }
        }
        Pattern::StringLiteral(literal) => literal.literal.clone(),
        Pattern::Product(product) => {
            let prefix = if product.mutable {
                "mut "
            } else if product.moved {
                "move "
            } else {
                ""
            };
            format!(
                "{}({})",
                prefix,
                product
                    .elements
                    .iter()
                    .map(render_pattern)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Pattern::Nominal(nominal) => format!(
            "{}{} {}",
            nominal
                .namespace
                .as_ref()
                .map(|namespace| format!("{namespace}."))
                .unwrap_or_default(),
            nominal.name,
            render_pattern(&nominal.argument),
        ),
        Pattern::Splice(splice) => format!("${}", splice.name),
    }
}

// --- expressions ------------------------------------------------------------

fn render_expr(program: &Program, expression: &Expression) -> String {
    if !expr_has_generated(expression) {
        return expression.syntax().text().trim().to_owned();
    }
    match expression {
        Expression::Block(block) => render_block(program, block),
        Expression::Match(match_) => {
            let mut out = format!("match {} {{\n", render_expr(program, &match_.subject));
            for arm in &match_.arms {
                let body = indent_lines(&render_expr(program, &arm.body), 1);
                out.push_str(&format!(
                    "    {} => {},\n",
                    render_pattern(&arm.pattern),
                    body.trim_start(),
                ));
            }
            out.push('}');
            out
        }
        Expression::Function(function) => format!(
            "{} => {}",
            render_pattern(&function.pattern),
            render_expr(program, &function.body),
        ),
        Expression::Loop(loop_) => format!("loop {}", render_block(program, &loop_.body)),
        Expression::Resource(resource) => format!("resource {}", resource.resource),
        Expression::With(with) => format!(
            "with {}{} = {} {}",
            if with.mutable { "mut " } else { "" },
            with.resource,
            render_expr(program, &with.value),
            render_block(program, &with.body),
        ),
        Expression::Product(product) => {
            let parts = product
                .elements
                .iter()
                .map(|element| {
                    let mut prefix = String::new();
                    if element.spread {
                        prefix.push_str(if element.named_spread { "...=" } else { "..." });
                    }
                    if let Some(name) = &element.name {
                        if element.designated {
                            prefix.push('.');
                        }
                        prefix.push_str(name);
                        prefix.push_str(": ");
                    }
                    format!("{prefix}{}", render_expr(program, &element.value))
                })
                .collect::<Vec<_>>();
            format!("({})", parts.join(", "))
        }
        Expression::RepeatedProduct(repeated) => {
            format!(
                "({}; {})",
                render_expr(program, &repeated.value),
                render_expr(program, &repeated.count),
            )
        }
        Expression::Call(call) => splice(program, &call.syntax, &[&call.callee, &call.argument]),
        Expression::Binary(binary) => format!(
            "{} {} {}",
            render_expr(program, &binary.left),
            binary.operator.text(),
            render_expr(program, &binary.right),
        ),
        Expression::Access(access) => {
            format!(
                "{}{}",
                render_expr(program, &access.value),
                accessor_suffix(&access.accessor),
            )
        }
        Expression::Index(index) => {
            format!(
                "{}[{}]",
                render_expr(program, &index.value),
                render_expr(program, &index.index)
            )
        }
        Expression::Logical(logical) => format!(
            "{} {} {}",
            render_expr(program, &logical.left),
            match logical.operator {
                LogicalOperator::And => "&&",
                LogicalOperator::Or => "||",
            },
            render_expr(program, &logical.right),
        ),
        Expression::Satisfies(satisfies) => {
            format!(
                "{} satisfies {}",
                render_expr(program, &satisfies.value),
                satisfies.ty
            )
        }
        _ => expression.syntax().text().trim().to_owned(),
    }
}

fn render_block(program: &Program, block: &BlockExpression) -> String {
    if !block_has_generated(block) {
        return block.syntax.text().trim().to_owned();
    }
    let mut inner = String::new();
    render_items(program, &block.items, 1, &mut inner);
    format!("{{\n{}\n}}", inner.trim_end())
}

fn accessor_suffix(accessor: &Accessor) -> String {
    match accessor {
        Accessor::Name(name) => format!(".{name}"),
        Accessor::Index(index) => format!(".{index}"),
        Accessor::Representation => ".*".to_owned(),
        Accessor::Method(method) => format!("^{method}"),
    }
}

/// Renders `<fixed prefix> <rendered value>`, where the prefix is `base`'s
/// tokens up to and including the last `separator` (the `=` of a binding, the
/// `return`/`break` keyword). Falls back to a plain splice if no separator is
/// present.
fn prefixed(program: &Program, base: &Syntax, separator: TokenKind, value: &Expression) -> String {
    let tokens = base.tokens();
    let split = match separator {
        // The binding `=`: the first one at bracket depth zero, so a default in
        // a type-parameter list (`<T = U>`) or a nested `let` in the body is
        // not mistaken for it.
        TokenKind::Equals => {
            let mut depth = 0i32;
            tokens.iter().position(|token| match token.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    depth += 1;
                    false
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    false
                }
                TokenKind::Operator if token.text == "<" => {
                    depth += 1;
                    false
                }
                TokenKind::Operator if token.text == ">" => {
                    depth -= 1;
                    false
                }
                TokenKind::Equals => depth == 0,
                _ => false,
            })
        }
        _ => tokens.iter().position(|token| token.kind == separator),
    };
    match split {
        Some(index) => {
            let prefix: String = tokens[..=index]
                .iter()
                .map(|token| token.text.as_str())
                .collect();
            format!("{} {}", prefix.trim(), render_expr(program, value))
        }
        None => splice(program, base, &[value]),
    }
}

// --- token-stream splice --------------------------------------------------

/// Emits `base`'s tokens, replacing the span of each changed child with its
/// rendered text. `children` must be in source order.
fn splice(program: &Program, base: &Syntax, children: &[&Expression]) -> String {
    let stream = base.token_stream();
    let range = base.token_range();
    if stream.is_empty() || range.start >= range.end {
        return children
            .iter()
            .map(|child| render_expr(program, child))
            .collect::<Vec<_>>()
            .join(" ");
    }

    // (absolute start, absolute end, child, anchored) — anchored children sit at
    // a known span; foreign children fill the gap up to the next anchor.
    let anchors = children
        .iter()
        .map(|&child| {
            if base.shares_token_stream(child.syntax()) {
                let child_range = child.syntax().token_range();
                (child_range.start, child_range.end, child, true)
            } else {
                (usize::MAX, usize::MAX, child, false)
            }
        })
        .collect::<Vec<_>>();

    let mut out = String::new();
    let mut cursor = range.start;
    for (index, &(start, end, child, anchored)) in anchors.iter().enumerate() {
        if anchored {
            out.push_str(&token_text(stream, cursor, start.max(cursor)));
            if expr_has_generated(child) {
                append_child(&mut out, &render_expr(program, child));
            } else {
                append_child(&mut out, &token_text(stream, start, end));
            }
            cursor = end.max(cursor);
        } else {
            let next_anchor = anchors[index + 1..]
                .iter()
                .find(|(_, _, _, anchored)| *anchored)
                .map(|&(start, ..)| start)
                .unwrap_or(range.end);
            let mut trivia = cursor;
            while trivia < next_anchor && stream[trivia].kind.is_trivia() {
                out.push_str(&stream[trivia].text);
                trivia += 1;
            }
            append_child(&mut out, &render_expr(program, child));
            cursor = next_anchor.max(cursor);
        }
    }
    out.push_str(&token_text(stream, cursor, range.end));
    out.trim().to_owned()
}

fn append_child(out: &mut String, child: &str) {
    if let Some(last) = out.chars().next_back()
        && !last.is_whitespace()
        && !matches!(last, '(' | '[' | '{' | '.')
    {
        out.push(' ');
    }
    out.push_str(child);
}

fn token_text(stream: &[SyntaxToken], start: usize, end: usize) -> String {
    let end = end.min(stream.len());
    if start >= end {
        return String::new();
    }
    stream[start..end]
        .iter()
        .map(|token| token.text.as_str())
        .collect()
}

// --- "contains a macro-generated node" queries ----------------------------

fn item_has_generated(item: &Item) -> bool {
    if item.syntax().is_generated() {
        return true;
    }
    match item {
        Item::Binding(binding) => binding.value.as_ref().is_some_and(expr_has_generated),
        Item::PatternBinding(binding) => expr_has_generated(&binding.value),
        Item::Assignment(assignment) => {
            expr_has_generated(&assignment.target) || expr_has_generated(&assignment.value)
        }
        Item::Return(item) => expr_has_generated(&item.value),
        Item::Break(item) => item.value.as_ref().is_some_and(expr_has_generated),
        Item::Expression(expression) => expr_has_generated(expression),
        Item::TraitImplementation(implementation) => implementation
            .members
            .iter()
            .any(|member| expr_has_generated(&member.value)),
        Item::TraitDeclaration(declaration) => declaration
            .members
            .iter()
            .any(|member| member.default.as_ref().is_some_and(expr_has_generated)),
        Item::Submodule(submodule) => submodule.module.items.iter().any(item_has_generated),
        Item::Modified(modified) => item_has_generated(&modified.item),
        Item::VisibilitySplice(splice) => item_has_generated(&splice.item),
        _ => false,
    }
}

fn block_has_generated(block: &BlockExpression) -> bool {
    block.syntax.is_generated() || block.items.iter().any(item_has_generated)
}

fn expr_has_generated(expression: &Expression) -> bool {
    if expression.syntax().is_generated() {
        return true;
    }
    match expression {
        Expression::Function(function) => expr_has_generated(&function.body),
        Expression::Satisfies(satisfies) => expr_has_generated(&satisfies.value),
        Expression::Match(match_) => {
            expr_has_generated(&match_.subject)
                || match_
                    .arms
                    .iter()
                    .any(|arm| expr_has_generated(&arm.body) || pattern_is_generated(&arm.pattern))
        }
        Expression::Loop(loop_) => block_has_generated(&loop_.body),
        Expression::With(with) => {
            expr_has_generated(&with.value) || block_has_generated(&with.body)
        }
        Expression::Block(block) => block_has_generated(block),
        Expression::Product(product) => product
            .elements
            .iter()
            .any(|element| expr_has_generated(&element.value)),
        Expression::RepeatedProduct(repeated) => {
            expr_has_generated(&repeated.value) || expr_has_generated(&repeated.count)
        }
        Expression::Call(call) => {
            expr_has_generated(&call.callee) || expr_has_generated(&call.argument)
        }
        Expression::Access(access) => expr_has_generated(&access.value),
        Expression::Index(index) => {
            expr_has_generated(&index.value) || expr_has_generated(&index.index)
        }
        Expression::Binary(binary) => {
            expr_has_generated(&binary.left) || expr_has_generated(&binary.right)
        }
        Expression::Logical(logical) => {
            expr_has_generated(&logical.left) || expr_has_generated(&logical.right)
        }
        _ => false,
    }
}

fn pattern_is_generated(pattern: &Pattern) -> bool {
    pattern.syntax().is_generated()
}
