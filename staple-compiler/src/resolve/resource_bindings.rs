//! Replace lexically resolved resource aliases with ordinary resource accesses.

use staple_syntax::{Expression, Item, ResourceExpression, StringTemplatePart, SyntaxId, Type};
use std::collections::HashMap;

type References = HashMap<SyntaxId, Type>;

pub(super) fn rewrite_items(items: &mut [Item], references: &References) {
    for item in items {
        match item {
            Item::Binding(binding) => {
                if let Some(value) = &mut binding.value {
                    rewrite_expression(value, references);
                }
            }
            Item::PatternBinding(binding) => rewrite_expression(&mut binding.value, references),
            Item::Assignment(assignment) => {
                rewrite_expression(&mut assignment.target, references);
                rewrite_expression(&mut assignment.value, references);
            }
            Item::Return(item) => rewrite_expression(&mut item.value, references),
            Item::Break(item) => {
                if let Some(value) = &mut item.value {
                    rewrite_expression(value, references);
                }
            }
            Item::Expression(expression) => rewrite_expression(expression, references),
            Item::Submodule(module) => rewrite_items(&mut module.module.items, references),
            Item::TraitDeclaration(declaration) => {
                for member in &mut declaration.members {
                    if let Some(default) = &mut member.default {
                        rewrite_expression(default, references);
                    }
                }
            }
            Item::TraitImplementation(implementation) => {
                for member in &mut implementation.members {
                    rewrite_expression(&mut member.value, references);
                }
            }
            // These cannot contain runtime references to resource bindings.
            Item::UseDeclaration(_)
            | Item::ExternBlock(_)
            | Item::TypeDeclaration(_)
            | Item::MacroDeclaration(_)
            | Item::Continue(_)
            | Item::Modified(_)
            | Item::VisibilityMacroInvocation(_)
            | Item::VisibilitySplice(_)
            | Item::RepeatedItemSplice(_) => {}
        }
    }
}

pub(super) fn rewrite_expression(expression: &mut Expression, references: &References) {
    if let Expression::Name(name) = expression
        && let Some(resource) = references.get(&name.syntax.id)
    {
        *expression = Expression::Resource(Box::new(ResourceExpression {
            syntax: name.syntax.clone(),
            resource: resource.clone(),
        }));
        return;
    }
    match expression {
        Expression::Function(function) => rewrite_expression(&mut function.body, references),
        Expression::TypeApplication(application) => {
            rewrite_expression(&mut application.value, references)
        }
        Expression::TypeAscription(ascription) => {
            rewrite_expression(&mut ascription.value, references)
        }
        Expression::Match(match_) => {
            rewrite_expression(&mut match_.subject, references);
            for arm in &mut match_.arms {
                rewrite_expression(&mut arm.body, references);
            }
        }
        Expression::Loop(loop_) => rewrite_items(&mut loop_.body.items, references),
        Expression::Coro(coro) => rewrite_items(&mut coro.body.items, references),
        Expression::Await(await_) => rewrite_expression(&mut await_.operand, references),
        Expression::Try(try_) => rewrite_expression(&mut try_.operand, references),
        Expression::Conversion(conversion) => rewrite_expression(&mut conversion.value, references),
        Expression::With(with) => {
            rewrite_expression(&mut with.value, references);
            rewrite_items(&mut with.body.items, references);
        }
        Expression::Block(block) => rewrite_items(&mut block.items, references),
        Expression::Product(product) => {
            for element in &mut product.elements {
                rewrite_expression(&mut element.value, references);
            }
        }
        Expression::RepeatedProduct(repeated) => {
            rewrite_expression(&mut repeated.value, references)
        }
        Expression::Call(call) => {
            rewrite_expression(&mut call.callee, references);
            rewrite_expression(&mut call.argument, references);
        }
        Expression::Access(access) => rewrite_expression(&mut access.value, references),
        Expression::Index(index) => {
            rewrite_expression(&mut index.value, references);
            rewrite_expression(&mut index.index, references);
        }
        Expression::Unary(unary) => rewrite_expression(&mut unary.operand, references),
        Expression::Binary(binary) => {
            rewrite_expression(&mut binary.left, references);
            rewrite_expression(&mut binary.right, references);
        }
        Expression::Pipe(pipe) => {
            rewrite_expression(&mut pipe.receiver, references);
            for argument in pipe.stage_arguments_mut() {
                rewrite_expression(argument, references);
            }
        }
        Expression::Logical(logical) => {
            rewrite_expression(&mut logical.left, references);
            rewrite_expression(&mut logical.right, references);
        }
        Expression::StringTemplate(template) => {
            for part in &mut template.parts {
                if let StringTemplatePart::Interpolation(interpolation) = part {
                    rewrite_expression(&mut interpolation.expression, references);
                }
            }
        }
        Expression::Resource(_)
        | Expression::Name(_)
        | Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
    }
}
