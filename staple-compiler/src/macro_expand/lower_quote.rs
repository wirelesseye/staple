//! Rewrites quotations into ordinary calls, so macro code can be checked as
//! the code it is.
//!
//! `quote { … }` becomes `__quote (id, splices)` and `parse_quote { … }`
//! becomes `Parse.parse_quote (id, splices)`. `id` indexes a table of quotation
//! fragments. `splices` lists the distinct names a fragment splices, in order
//! of first use, each as `Splice.splice name`, so splicing a value that is not
//! syntax is a type error.

use staple_syntax::{
    Accessor, BlockExpression, CallExpression, Expression, IntegerExpression, Item, NameExpression,
    NamedType, ProductElement, ProductExpression, QuoteExpression, QuoteKind, Syntax, SyntaxId,
    SyntaxToken, TokenKind, Type, TypeApplicationExpression,
};

use crate::ModuleId;

/// The names a quotation's tokens splice, in order and with repeats. A splice
/// may put trivia between `$` and its name.
pub(super) fn splice_names(tokens: &[SyntaxToken]) -> impl Iterator<Item = &str> {
    tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| token.kind == TokenKind::Dollar)
        .filter_map(|(index, _)| {
            let next = tokens[index + 1..].iter().find(|next| !next.kind.is_trivia())?;
            (next.kind == TokenKind::Identifier).then_some(next.text.as_str())
        })
}

/// The text of each quotation lowered so far, by template id.
#[derive(Default)]
pub(crate) struct TemplateTable {
    pub(crate) templates: Vec<String>,
}

pub(super) struct QuoteLowering<'a> {
    /// The module that defines the functions the lowered code calls.
    pub(super) syntax_module: ModuleId,
    pub(super) next_syntax_id: &'a mut usize,
    pub(super) table: &'a mut TemplateTable,
}

impl QuoteLowering<'_> {
    fn syntax(&mut self, like: &Syntax) -> Syntax {
        let mut syntax = like.clone();
        syntax.id = SyntaxId(*self.next_syntax_id);
        *self.next_syntax_id += 1;
        syntax
    }

    fn name(&mut self, like: &Syntax, text: &str) -> Expression {
        let mut syntax = self.syntax(like);
        syntax.definition_module = Some(self.syntax_module.0);
        Expression::Name(NameExpression {
            syntax,
            name: text.to_owned(),
        })
    }

    /// A name written by the quoting code, which resolves where it was written.
    fn local(&mut self, like: &Syntax, text: &str) -> Expression {
        Expression::Name(NameExpression {
            syntax: self.syntax(like),
            name: text.to_owned(),
        })
    }

    fn call(&mut self, like: &Syntax, callee: Expression, argument: Expression) -> Expression {
        Expression::Call(CallExpression {
            spread: false,
            syntax: self.syntax(like),
            callee: Box::new(callee),
            argument: Box::new(argument),
        })
    }

    /// The `Syntax` type as written by the quoting module, so a generated
    /// sequence has a concrete element type and needs no inference.
    fn syntax_type(&mut self, like: &Syntax) -> Type {
        let mut syntax = self.syntax(like);
        syntax.definition_module = Some(self.syntax_module.0);
        Type::Named(NamedType {
            syntax,
            namespace: None,
            name: "Syntax".to_owned(),
        })
    }

    fn sequence(&mut self, like: &Syntax, elements: Vec<(Option<&str>, Expression)>) -> Expression {
        let constructor = self.name(like, "Sequence");
        let constructor =
            Expression::TypeApplication(Box::new(TypeApplicationExpression {
                syntax: self.syntax(like),
                value: Box::new(constructor),
                arguments: vec![self.syntax_type(like)],
            }));
        let product = self.product(like, false, elements);
        self.call(like, constructor, product)
    }

    fn product(
        &mut self,
        like: &Syntax,
        slotted: bool,
        elements: Vec<(Option<&str>, Expression)>,
    ) -> Expression {
        let elements = elements
            .into_iter()
            .map(|(name, value)| ProductElement {
                syntax: self.syntax(like),
                name: name.map(str::to_owned),
                designated: false,
                value,
                spread: false,
                named_spread: false,
            })
            .collect();
        Expression::Product(ProductExpression {
            slotted,
            syntax: self.syntax(like),
            elements,
        })
    }

    /// The names a quotation splices, in order of first use.
    fn splices(quote: &QuoteExpression) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for name in splice_names(quote.contents.tokens()) {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
        names
    }

    fn lower_quote(&mut self, quote: &QuoteExpression) -> Expression {
        let like = &quote.syntax;
        let id = self.table.templates.len();
        self.table.templates.push(quote.contents.text());
        let mut list = self.sequence(like, Vec::new());
        for name in Self::splices(quote).into_iter().rev() {
            let splice = {
                let trait_ = self.name(like, "Splice");
                Expression::Access(staple_syntax::AccessExpression {
                    syntax: self.syntax(like),
                    value: Box::new(trait_),
                    accessor: Accessor::Name("splice".to_owned()),
                })
            };
            let value = self.local(like, &name);
            let spliced = self.call(like, splice, value);
            list = self.sequence(
                like,
                vec![(Some("first"), spliced), (Some("rest"), list)],
            );
        }
        let template = Expression::Integer(IntegerExpression {
            syntax: self.syntax(like),
            literal: id.to_string(),
        });
        let arguments = self.product(like, true, vec![(None, template), (None, list)]);
        let callee = match quote.kind {
            QuoteKind::Quote => self.name(like, "__quote"),
            QuoteKind::ParseQuote => {
                let trait_ = self.name(like, "Parse");
                Expression::Access(staple_syntax::AccessExpression {
                    syntax: self.syntax(like),
                    value: Box::new(trait_),
                    accessor: Accessor::Name("parse_quote".to_owned()),
                })
            }
        };
        // The two slots are passed as consecutive arguments.
        let call = self.call(like, callee, arguments);
        match call {
            Expression::Call(mut call) => {
                call.spread = true;
                Expression::Call(call)
            }
            call => call,
        }
    }

    pub(super) fn lower_item(&mut self, item: &mut Item) {
        match item {
            Item::Modified(modified) => self.lower_item(&mut modified.item),
            Item::VisibilitySplice(splice) => self.lower_item(&mut splice.item),
            Item::ExternBlock(block) => {
                for binding in &mut block.bindings {
                    if let Some(value) = &mut binding.value {
                        self.lower_expression(value);
                    }
                }
            }
            Item::TraitDeclaration(declaration) => {
                for member in &mut declaration.members {
                    if let Some(default) = &mut member.default {
                        self.lower_expression(default);
                    }
                }
            }
            Item::TraitImplementation(implementation) => {
                for member in &mut implementation.members {
                    self.lower_expression(&mut member.value);
                }
            }
            Item::Binding(binding) => {
                if let Some(value) = &mut binding.value {
                    self.lower_expression(value);
                }
            }
            Item::PatternBinding(binding) => self.lower_expression(&mut binding.value),
            Item::Assignment(assignment) => {
                self.lower_expression(&mut assignment.target);
                self.lower_expression(&mut assignment.value);
            }
            Item::Return(return_) => self.lower_expression(&mut return_.value),
            Item::Break(break_) => {
                if let Some(value) = &mut break_.value {
                    self.lower_expression(value);
                }
            }
            Item::Expression(expression) => self.lower_expression(expression),
            Item::Submodule(submodule) => {
                for item in &mut submodule.module.items {
                    self.lower_item(item);
                }
            }
            Item::MacroDeclaration(_)
            | Item::VisibilityMacroInvocation(_)
            | Item::RepeatedItemSplice(_)
            | Item::Continue(_)
            | Item::TypeDeclaration(_)
            | Item::UseDeclaration(_) => {}
        }
    }

    fn lower_block(&mut self, block: &mut BlockExpression) {
        for item in &mut block.items {
            self.lower_item(item);
        }
    }

    pub(super) fn lower_expression(&mut self, expression: &mut Expression) {
        match expression {
            Expression::Quote(quote) => {
                *expression = self.lower_quote(quote);
            }
            Expression::Binary(binary) => {
                self.lower_expression(&mut binary.left);
                self.lower_expression(&mut binary.right);
            }
            Expression::Unary(unary) => self.lower_expression(&mut unary.operand),
            Expression::Pipe(pipe) => {
                self.lower_expression(&mut pipe.receiver);
                self.lower_expression(&mut pipe.stage);
            }
            Expression::Function(function) => self.lower_expression(&mut function.body),
            Expression::TypeApplication(application) => self.lower_expression(&mut application.value),
            Expression::TypeAscription(ascription) => self.lower_expression(&mut ascription.value),
            Expression::Match(match_) => {
                self.lower_expression(&mut match_.subject);
                for arm in &mut match_.arms {
                    self.lower_expression(&mut arm.body);
                }
            }
            Expression::Loop(loop_) => self.lower_block(&mut loop_.body),
            Expression::Coro(coro) => self.lower_block(&mut coro.body),
            Expression::Await(await_) => self.lower_expression(&mut await_.operand),
            Expression::Try(try_) => self.lower_expression(&mut try_.operand),
            Expression::Conversion(conversion) => self.lower_expression(&mut conversion.value),
            Expression::With(with) => {
                self.lower_expression(&mut with.value);
                self.lower_block(&mut with.body);
            }
            Expression::Block(block) => self.lower_block(block),
            Expression::Product(product) => {
                for element in &mut product.elements {
                    self.lower_expression(&mut element.value);
                }
            }
            Expression::RepeatedProduct(repeated) => self.lower_expression(&mut repeated.value),
            Expression::Call(call) => {
                self.lower_expression(&mut call.callee);
                self.lower_expression(&mut call.argument);
            }
            Expression::Access(access) => self.lower_expression(&mut access.value),
            Expression::Index(index) => {
                self.lower_expression(&mut index.value);
                self.lower_expression(&mut index.index);
            }
            Expression::Logical(logical) => {
                self.lower_expression(&mut logical.left);
                self.lower_expression(&mut logical.right);
            }
            Expression::StringTemplate(template) => {
                for part in &mut template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        self.lower_expression(&mut interpolation.expression);
                    }
                }
            }
            Expression::Resource(_)
            | Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Char(_)
            | Expression::Byte(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }
}
