//! Reconstructs source text for the entry module after macro expansion, for
//! `stpl expand`.
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
//! Generated ([`Syntax::is_generated`]) nodes are emitted from their own
//! tokens. Known limitations:
//!
//!   * A further non-wholesale expansion nested inside a generated node is not
//!     re-spliced.
//!   * A `parse_quote` that splices a name or type into a *declaration* (for
//!     example `parse_quote { type alias $name = $ty }`) renders with the
//!     `$name` / `$ty` placeholders still in place; substituting them needs a
//!     type/pattern unparser this module deliberately avoids.
//!   * Macros inside a hand-written inline `mod` block render unexpanded (their
//!     expansion lives in a separate flattened module).

use stapler::{
    Accessor, BlockExpression, Expression, Item, LogicalOperator, Pattern, Program, Submodule,
    Syntax, SyntaxToken, TokenKind, Visibility,
};

/// Renders the entry module of `program` (already macro-expanded) as source.
pub fn render_module(program: &Program) -> String {
    let module = &program.module(program.entry()).syntax;
    let mut output = String::new();
    render_items(&module.items, 0, &mut output);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn render_items(items: &[Item], indent: usize, output: &mut String) {
    for item in items {
        if let Item::Submodule(submodule) = item {
            output.push_str(&indent_lines(&submodule_header(submodule), indent));
            output.push('\n');
            render_items(&submodule.module.items, indent + 1, output);
            output.push_str(&indent_lines("}", indent));
            output.push_str("\n\n");
            continue;
        }
        let text = render_item(item);
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

fn render_item(item: &Item) -> String {
    if !item_has_generated(item) {
        return item.syntax().text().trim().to_owned();
    }
    match item {
        Item::Expression(expression) => render_expr(expression),
        Item::Binding(binding) => match &binding.value {
            Some(value) => prefixed(&binding.syntax, TokenKind::Equals, value),
            None => binding.syntax.text().trim().to_owned(),
        },
        Item::PatternBinding(binding) => {
            prefixed(&binding.syntax, TokenKind::Equals, &binding.value)
        }
        Item::Assignment(assignment) => {
            prefixed(&assignment.syntax, TokenKind::Equals, &assignment.value)
        }
        Item::Return(item) => prefixed(&item.syntax, TokenKind::Return, &item.value),
        Item::Break(item) => match &item.value {
            Some(value) => prefixed(&item.syntax, TokenKind::Break, value),
            None => item.syntax.text().trim().to_owned(),
        },
        Item::TraitImplementation(implementation) => {
            let children = implementation
                .members
                .iter()
                .map(|member| &member.value)
                .collect::<Vec<_>>();
            splice(&implementation.syntax, &children)
        }
        Item::TraitDeclaration(declaration) => {
            let children = declaration
                .members
                .iter()
                .filter_map(|member| member.default.as_ref())
                .collect::<Vec<_>>();
            splice(&declaration.syntax, &children)
        }
        Item::Modified(modified) => render_item(&modified.item),
        Item::VisibilitySplice(splice) => render_item(&splice.item),
        _ => item.syntax().text().trim().to_owned(),
    }
}

// --- expressions ------------------------------------------------------------

fn render_expr(expression: &Expression) -> String {
    if !expr_has_generated(expression) {
        return expression.syntax().text().trim().to_owned();
    }
    match expression {
        Expression::Block(block) => render_block(block),
        Expression::Match(match_) => {
            let mut out = format!("match {} {{\n", render_expr(&match_.subject));
            for arm in &match_.arms {
                let body = indent_lines(&render_expr(&arm.body), 1);
                out.push_str(&format!(
                    "    {} => {},\n",
                    arm.pattern.syntax().text().trim(),
                    body.trim_start(),
                ));
            }
            out.push('}');
            out
        }
        Expression::Function(function) => format!(
            "{} => {}",
            function.pattern.syntax().text().trim(),
            render_expr(&function.body),
        ),
        Expression::Loop(loop_) => format!("loop {}", render_block(&loop_.body)),
        Expression::With(with) => format!(
            "with {}{} = {} {}",
            if with.mutable { "mut " } else { "" },
            with.resource.syntax().text().trim(),
            render_expr(&with.value),
            render_block(&with.body),
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
                        prefix.push_str(name);
                        prefix.push_str(": ");
                    }
                    format!("{prefix}{}", render_expr(&element.value))
                })
                .collect::<Vec<_>>();
            format!("({})", parts.join(", "))
        }
        Expression::Call(call) => match desugared_operator(call) {
            Some((left, operator, right)) => {
                format!("{} {operator} {}", render_expr(left), render_expr(right))
            }
            None => splice(&call.syntax, &[&call.callee, &call.argument]),
        },
        Expression::Access(access) => {
            format!(
                "{}{}",
                render_expr(&access.value),
                accessor_suffix(&access.accessor),
            )
        }
        Expression::Index(index) => {
            format!(
                "{}[{}]",
                render_expr(&index.value),
                render_expr(&index.index)
            )
        }
        Expression::Logical(logical) => format!(
            "{} {} {}",
            render_expr(&logical.left),
            match logical.operator {
                LogicalOperator::And => "&&",
                LogicalOperator::Or => "||",
            },
            render_expr(&logical.right),
        ),
        Expression::Satisfies(satisfies) => splice(&satisfies.syntax, &[&satisfies.value]),
        _ => expression.syntax().text().trim().to_owned(),
    }
}

fn render_block(block: &BlockExpression) -> String {
    if !block_has_generated(block) {
        return block.syntax.text().trim().to_owned();
    }
    let mut inner = String::new();
    render_items(&block.items, 1, &mut inner);
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

/// Recognizes the shape the parser desugars a binary operator into
/// (`Trait.method left` applied to `right`, with the synthetic callee's tokens
/// being the operator itself) and returns the operands and operator text.
fn desugared_operator(
    call: &stapler::CallExpression,
) -> Option<(&Expression, String, &Expression)> {
    let Expression::Call(inner) = call.callee.as_ref() else {
        return None;
    };
    let left = inner.argument.as_ref();
    let right = call.argument.as_ref();
    let operator = match inner.callee.as_ref() {
        Expression::Access(access) => {
            let text = access.syntax.text().trim().to_owned();
            matches!(
                text.as_str(),
                "+" | "-" | "*" | "/" | "==" | "!=" | "<=" | ">=" | "<" | ">"
            )
            .then_some(text)?
        }
        Expression::Name(name) => {
            let text = name.syntax.text().trim().to_owned();
            matches!(text.as_str(), ".." | "..=").then_some(text)?
        }
        _ => return None,
    };
    Some((left, operator, right))
}

/// Renders `<fixed prefix> <rendered value>`, where the prefix is `base`'s
/// tokens up to and including the last `separator` (the `=` of a binding, the
/// `return`/`break` keyword). Falls back to a plain splice if no separator is
/// present.
fn prefixed(base: &Syntax, separator: TokenKind, value: &Expression) -> String {
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
            format!("{} {}", prefix.trim(), render_expr(value))
        }
        None => splice(base, &[value]),
    }
}

// --- token-stream splice --------------------------------------------------

/// Emits `base`'s tokens, replacing the span of each changed child with its
/// rendered text. `children` must be in source order.
fn splice(base: &Syntax, children: &[&Expression]) -> String {
    let stream = base.token_stream();
    let range = base.token_range();
    if stream.is_empty() || range.start >= range.end {
        return children
            .iter()
            .map(|child| render_expr(child))
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
                append_child(&mut out, &render_expr(child));
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
            append_child(&mut out, &render_expr(child));
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
        Expression::Call(call) => {
            expr_has_generated(&call.callee) || expr_has_generated(&call.argument)
        }
        Expression::Access(access) => expr_has_generated(&access.value),
        Expression::Index(index) => {
            expr_has_generated(&index.value) || expr_has_generated(&index.index)
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
