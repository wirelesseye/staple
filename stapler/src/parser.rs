use crate::ast::*;
use crate::lexer::lex;
use std::sync::Arc;

/// Parses a complete Staple source file into a lossless syntax tree.
pub fn parse(source: &str) -> Result<Module, ParseError> {
    let tokens = Arc::from(lex(source));
    Grammar::new(tokens, Arc::from(source), 0, None).parse_source_file()
}

/// Parses a named source while assigning syntax IDs from a shared sequence.
///
/// On success, `next_syntax_id` is advanced past every ID assigned to the
/// module. The source name is retained in the spans produced by the parser.
pub(crate) fn parse_with_syntax_ids(
    source: &str,
    next_syntax_id: &mut usize,
    source_name: &str,
) -> Result<Module, ParseError> {
    let tokens = Arc::from(lex(source));
    let module = Grammar::new(
        tokens,
        Arc::from(source),
        *next_syntax_id,
        Some(Arc::from(source_name)),
    )
    .parse_source_file()?;
    *next_syntax_id = module.syntax.id.0 + 1;
    Ok(module)
}

/// Reinterprets one macro argument's original tokens as exactly one type.
pub(crate) fn parse_type_fragment(
    syntax: &Syntax,
    grouped: bool,
    next_syntax_id: &mut usize,
) -> Result<Type, ParseError> {
    parse_fragment(syntax, grouped, next_syntax_id, |grammar| {
        grammar.parse_type()
    })
}

/// Reinterprets quotation contents as a type template, permitting splices.
pub(crate) fn parse_type_template_fragment(
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Result<Type, ParseError> {
    parse_fragment(syntax, false, next_syntax_id, |grammar| {
        grammar.quote_depth += 1;
        grammar.parse_type()
    })
}

/// Reinterprets one macro argument's original tokens as exactly one pattern.
pub(crate) fn parse_pattern_fragment(
    syntax: &Syntax,
    grouped: bool,
    next_syntax_id: &mut usize,
) -> Result<Pattern, ParseError> {
    parse_fragment(syntax, grouped, next_syntax_id, |grammar| {
        grammar.parse_pattern()
    })
}

/// Reinterprets quotation contents as a pattern template, permitting splices.
pub(crate) fn parse_pattern_template_fragment(
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Result<Pattern, ParseError> {
    parse_fragment(syntax, false, next_syntax_id, |grammar| {
        grammar.quote_depth += 1;
        grammar.parse_pattern()
    })
}

/// Reinterprets original tokens as exactly one expression.
pub(crate) fn parse_expression_fragment(
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Result<Expression, ParseError> {
    parse_fragment(syntax, false, next_syntax_id, |grammar| {
        grammar.parse_expression()
    })
}

/// Reinterprets original tokens as exactly one item.
pub(crate) fn parse_item_fragment(
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Result<Item, ParseError> {
    parse_fragment(syntax, false, next_syntax_id, |grammar| {
        grammar.parse_item()
    })
}

/// Reinterprets original tokens as a complete list of zero or more items.
pub(crate) fn parse_item_list_fragment(
    syntax: &Syntax,
    next_syntax_id: &mut usize,
) -> Result<Vec<Item>, ParseError> {
    parse_fragment(syntax, false, next_syntax_id, |grammar| {
        let mut items = Vec::new();
        while grammar.peek().is_some() {
            items.push(grammar.parse_item()?);
            grammar.eat(TokenKind::Semicolon);
        }
        Ok(items)
    })
}

fn parse_fragment<T>(
    syntax: &Syntax,
    grouped: bool,
    next_syntax_id: &mut usize,
    parse: impl FnOnce(&mut Grammar) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    let all_tokens = &syntax.tokens;
    let source = all_tokens
        .iter()
        .map(|token| token.text.as_str())
        .collect::<String>();
    let mut tokens = syntax.tokens().to_vec();
    if grouped {
        let first = tokens
            .iter()
            .position(|token| !token.kind.is_trivia())
            .filter(|index| tokens[*index].kind == TokenKind::LParen)
            .ok_or_else(|| fragment_error(syntax, "expected grouped syntax argument"))?;
        let last = tokens
            .iter()
            .rposition(|token| !token.kind.is_trivia())
            .filter(|index| tokens[*index].kind == TokenKind::RParen)
            .ok_or_else(|| fragment_error(syntax, "expected grouped syntax argument"))?;
        tokens = tokens[first + 1..last].to_vec();
    }
    let source_name = match &syntax.span {
        Span::User { source, .. } => source.clone(),
        Span::Compiler => None,
    };
    let mut grammar = Grammar::new(
        Arc::from(tokens),
        Arc::from(source),
        *next_syntax_id,
        source_name,
    );
    let value = parse(&mut grammar)?;
    if grammar.peek().is_some() {
        return Err(grammar.error("expected one complete syntax argument"));
    }
    *next_syntax_id = grammar.next_syntax_id;
    Ok(value)
}

fn fragment_error(syntax: &Syntax, message: &'static str) -> ParseError {
    let (offset, location) = match &syntax.span {
        Span::User {
            range, location, ..
        } => (
            range.start,
            location.unwrap_or(SourceLocation { line: 1, column: 1 }),
        ),
        Span::Compiler => (0, SourceLocation { line: 1, column: 1 }),
    };
    ParseError {
        offset,
        location,
        message: message.to_owned(),
    }
}

#[derive(Clone)]
struct Grammar {
    tokens: Arc<[SyntaxToken]>,
    position: usize,
    source: Arc<str>,
    source_len: usize,
    line_starts: Vec<usize>,
    next_syntax_id: usize,
    newline_terminates_expression: bool,
    newline_terminates_type: bool,
    any_newline_terminates_type: bool,
    brace_terminates_expression: bool,
    macro_punctuation_arguments: bool,
    quote_depth: usize,
    source_name: Option<Arc<str>>,
}

impl Grammar {
    /// Creates a grammar cursor over `tokens` with the given syntax metadata.
    fn new(
        tokens: Arc<[SyntaxToken]>,
        source: Arc<str>,
        next_syntax_id: usize,
        source_name: Option<Arc<str>>,
    ) -> Self {
        let line_starts = line_starts(&source);
        let source_len = source.len();
        Self {
            tokens,
            position: 0,
            source,
            source_len,
            line_starts,
            next_syntax_id,
            newline_terminates_expression: false,
            newline_terminates_type: false,
            any_newline_terminates_type: false,
            brace_terminates_expression: false,
            macro_punctuation_arguments: false,
            quote_depth: 0,
            source_name,
        }
    }

    /// Parses all top-level items and returns the completed module.
    fn parse_source_file(mut self) -> Result<Module, ParseError> {
        let start = self.position;
        let mut items = Vec::new();
        while self.peek().is_some() {
            items.push(self.parse_item()?);
            self.eat(TokenKind::Semicolon);
        }
        self.position = self.tokens.len();
        Ok(Module {
            syntax: self.syntax(start),
            items,
        })
    }

    /// Parses one top-level declaration or statement.
    fn parse_item(&mut self) -> Result<Item, ParseError> {
        let previous = self.newline_terminates_expression;
        self.newline_terminates_expression = true;
        let item_start = self.position;
        if self.at(TokenKind::At) {
            let mut modifiers = Vec::new();
            while self.at(TokenKind::At) {
                modifiers.push(self.parse_modifier_invocation()?);
            }
            let item = Box::new(self.parse_item()?);
            self.newline_terminates_expression = previous;
            return Ok(Item::Modified(ModifiedItem {
                syntax: self.syntax(item_start),
                modifiers,
                item,
            }));
        }
        if self.quote_depth > 0 && self.at(TokenKind::Dollar) {
            self.expect(TokenKind::Dollar, "expected `$`")?;
            let name = self
                .expect(
                    TokenKind::Identifier,
                    "expected a visibility splice name after `$`",
                )?
                .text;
            if self.eat(TokenKind::Ellipsis) {
                self.newline_terminates_expression = previous;
                return Ok(Item::RepeatedItemSplice(crate::RepeatedItemSplice {
                    syntax: self.syntax(item_start),
                    name,
                }));
            }
            let item = Box::new(self.parse_item()?);
            self.newline_terminates_expression = previous;
            return Ok(Item::VisibilitySplice(VisibilitySplice {
                syntax: self.syntax(item_start),
                name,
                item,
            }));
        }
        let visibility = if self.eat(TokenKind::Pub) {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let representation_visibility =
            if visibility == Visibility::Public && self.eat(TokenKind::LParen) {
                let modifier = self.expect(
                    TokenKind::Identifier,
                    "expected `repr` in visibility modifier",
                )?;
                if modifier.text != "repr" {
                    return Err(self.error("expected `repr` in visibility modifier"));
                }
                self.expect(TokenKind::RParen, "expected `)` after `repr`")?;
                Visibility::Public
            } else {
                Visibility::Private
            };
        let visibility_syntax = if representation_visibility == Visibility::Public {
            Some(VisibilitySyntax {
                syntax: self.syntax(item_start),
                kind: VisibilityKind::PublicRepr,
            })
        } else if visibility == Visibility::Public {
            Some(VisibilitySyntax {
                syntax: self.syntax(item_start),
                kind: VisibilityKind::Public,
            })
        } else {
            None
        };
        if let Some(visibility) = visibility_syntax
            && self.peek() == Some(TokenKind::Identifier)
        {
            let previous_macro_punctuation = self.macro_punctuation_arguments;
            self.macro_punctuation_arguments = true;
            let expression = self.parse_expression();
            self.macro_punctuation_arguments = previous_macro_punctuation;
            let expression = expression?;
            self.newline_terminates_expression = previous;
            return Ok(Item::VisibilityMacroInvocation(VisibilityMacroInvocation {
                syntax: self.syntax(item_start),
                visibility,
                expression,
            }));
        }
        if representation_visibility == Visibility::Public && self.peek() != Some(TokenKind::Type) {
            return Err(self.error("`pub(repr)` may only modify a type declaration"));
        }
        let item = match self.peek() {
            Some(TokenKind::Use) => self
                .parse_use_declaration(visibility, item_start)
                .map(Item::UseDeclaration),
            Some(TokenKind::Mod) => self
                .parse_submodule(visibility, item_start)
                .map(Item::Submodule),
            Some(TokenKind::Companion) if visibility == Visibility::Private => self
                .parse_companion(item_start)
                .map(Item::Submodule),
            Some(TokenKind::Companion) => Err(self.error("companion blocks cannot be public")),
            Some(TokenKind::Extern) => self
                .parse_extern_block(visibility, item_start)
                .map(Item::ExternBlock),
            Some(TokenKind::Type) => self
                .parse_type_declaration(visibility, representation_visibility, item_start)
                .map(Item::TypeDeclaration),
            Some(TokenKind::Macro) => self
                .parse_macro_declaration(visibility, item_start)
                .map(Item::MacroDeclaration),
            Some(TokenKind::Trait) => self
                .parse_trait_declaration(visibility, item_start)
                .map(Item::TraitDeclaration),
            Some(TokenKind::Impl) if visibility == Visibility::Private => self
                .parse_trait_implementation(item_start)
                .map(Item::TraitImplementation),
            Some(TokenKind::Impl) => Err(self.error("trait implementations cannot be public")),
            _ => self.parse_statement_with_visibility(visibility, Some(item_start)),
        };
        self.newline_terminates_expression = previous;
        item
    }

    fn parse_modifier_invocation(&mut self) -> Result<ModifierInvocation, ParseError> {
        let start = self.position;
        self.expect(TokenKind::At, "expected `@`")?;
        let first = self
            .expect(
                TokenKind::Identifier,
                "expected modifier macro name after `@`",
            )?
            .text;
        let (namespace, name) =
            self.parse_qualified_name_from(first, "expected modifier name after namespace")?;
        let argument = self
            .at(TokenKind::LParen)
            .then(|| self.parse_modifier_argument())
            .transpose()?;
        Ok(ModifierInvocation {
            syntax: self.syntax(start),
            namespace,
            name,
            argument,
        })
    }

    fn parse_modifier_argument(&mut self) -> Result<ModifierArgument, ParseError> {
        let start = self.position;
        let next_syntax_id = self.next_syntax_id;
        self.expect(TokenKind::LParen, "expected `(` after modifier name")?;
        let expression = self.parse_expression();
        if let Ok(expression) = expression
            && self.at(TokenKind::RParen)
        {
            self.expect(TokenKind::RParen, "expected `)` after modifier argument")?;
            return Ok(ModifierArgument {
                syntax: self.syntax(start),
                expression: Some(expression),
            });
        }
        self.position = start;
        self.next_syntax_id = next_syntax_id;
        let Expression::SyntaxArgument(argument) = self.parse_syntax_argument()? else {
            unreachable!("syntax argument parser must produce a syntax argument")
        };
        Ok(ModifierArgument {
            syntax: argument.syntax,
            expression: None,
        })
    }

    fn parse_submodule(
        &mut self,
        visibility: Visibility,
        start: usize,
    ) -> Result<Submodule, ParseError> {
        self.expect(TokenKind::Mod, "expected `mod`")?;
        let name = self.parse_quoted_identifier("expected submodule name")?;
        self.expect(TokenKind::LBrace, "expected `{` after submodule name")?;
        let module_start = self.position;
        let mut items = Vec::new();
        while self.peek().is_some() && !self.at(TokenKind::RBrace) {
            items.push(self.parse_item()?);
            self.eat(TokenKind::Semicolon);
        }
        if self.peek().is_none() {
            return Err(self.error("expected `}` after submodule items"));
        }
        let module = Module {
            syntax: self.syntax(module_start),
            items,
        };
        self.expect(TokenKind::RBrace, "expected `}` after submodule items")?;
        Ok(Submodule {
            syntax: self.syntax(start),
            visibility,
            name,
            module,
            companion: false,
            type_parameters: Vec::new(),
            trait_bounds: Vec::new(),
            subtype_bounds: Vec::new(),
            companion_target: None,
        })
    }

    fn parse_companion(&mut self, start: usize) -> Result<Submodule, ParseError> {
        self.expect(TokenKind::Companion, "expected `companion`")?;
        let (type_parameters, trait_bounds, subtype_bounds) = self.parse_bracketed_generics()?;
        let target = self.parse_type()?;
        let name = companion_target_name(&target)
            .ok_or_else(|| self.error("companion target must be a named type or type alias"))?;
        self.expect(TokenKind::LBrace, "expected `{` after companion target")?;
        let module_start = self.position;
        let mut items = Vec::new();
        while self.peek().is_some() && !self.at(TokenKind::RBrace) {
            items.push(self.parse_item()?);
            self.eat(TokenKind::Semicolon);
        }
        if self.peek().is_none() {
            return Err(self.error("expected `}` after companion items"));
        }
        for item in &mut items {
            if let Item::Binding(binding) = item {
                let mut parameters = type_parameters.clone();
                parameters.extend(binding.type_parameters.clone());
                binding.type_parameters = parameters;
                let mut bounds = trait_bounds.clone();
                bounds.extend(binding.trait_bounds.clone());
                binding.trait_bounds = bounds;
                let mut bounds = subtype_bounds.clone();
                bounds.extend(binding.subtype_bounds.clone());
                binding.subtype_bounds = bounds;
            }
        }
        let module = Module { syntax: self.syntax(module_start), items };
        self.expect(TokenKind::RBrace, "expected `}` after companion items")?;
        Ok(Submodule {
            syntax: self.syntax(start),
            visibility: Visibility::Public,
            name,
            module,
            companion: true,
            type_parameters,
            trait_bounds,
            subtype_bounds,
            companion_target: Some(target),
        })
    }

    fn parse_macro_declaration(
        &mut self,
        visibility: Visibility,
        start: usize,
    ) -> Result<MacroDeclaration, ParseError> {
        self.expect(TokenKind::Macro, "expected `macro`")?;
        let modifier = self.eat(TokenKind::At);
        let name = self
            .expect(TokenKind::Identifier, "expected macro name")?
            .text;
        let mut type_parameters = Vec::new();
        let mut trait_bounds = Vec::new();
        let mut subtype_bounds = Vec::new();
        let annotation = if self.eat(TokenKind::Colon) {
            let previous = self.newline_terminates_type;
            self.newline_terminates_type = true;
            let (parameters, bounds, subtypes) = self.parse_bracketed_generics()?;
            type_parameters = parameters;
            trait_bounds = bounds;
            subtype_bounds = subtypes;
            let annotation = self.parse_type();
            self.newline_terminates_type = previous;
            Some(annotation?)
        } else {
            None
        };
        let value = if self.eat(TokenKind::Equals) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(MacroDeclaration {
            syntax: self.syntax(start),
            visibility,
            name,
            modifier,
            type_parameters,
            trait_bounds,
            subtype_bounds,
            annotation,
            value,
        })
    }

    fn parse_trait_declaration(
        &mut self,
        visibility: Visibility,
        start: usize,
    ) -> Result<TraitDeclaration, ParseError> {
        self.expect(TokenKind::Trait, "expected `trait`")?;
        let name = self
            .expect(TokenKind::Identifier, "expected trait name")?
            .text;
        let (mut type_parameters, default_bounds) = self.parse_juxtaposed_type_parameters()?;
        if type_parameters.is_empty() {
            return Err(self.error("expected at least one trait type parameter"));
        }
        let (prerequisites, subtype_bounds, functional_dependencies) =
            self.parse_where_clause(&mut type_parameters, true)?;
        self.expect(TokenKind::LBrace, "expected `{` before trait members")?;
        let mut members = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated trait declaration"));
            }
            let member_start = self.position;
            let member_name = self
                .expect(TokenKind::Identifier, "expected trait member name")?
                .text;
            self.expect(TokenKind::Colon, "expected `:` after trait member name")?;
            let previous = self.newline_terminates_type;
            self.newline_terminates_type = true;
            let annotation = self.parse_type();
            self.newline_terminates_type = previous;
            let annotation = annotation?;
            let default = if self.eat(TokenKind::Equals) {
                Some(self.parse_expression()?)
            } else {
                None
            };
            members.push(TraitMember {
                syntax: self.syntax(member_start),
                name: member_name,
                annotation,
                default,
            });
            self.eat(TokenKind::Semicolon);
        }
        self.expect(TokenKind::RBrace, "expected `}` after trait members")?;
        Ok(TraitDeclaration {
            syntax: self.syntax(start),
            visibility,
            name,
            type_parameters,
            functional_dependencies,
            prerequisites,
            subtype_bounds,
            default_bounds,
            members,
        })
    }

    /// Parses the unified `where` clause shared by every generic-parameter
    /// header: trait bounds, subtype bounds, `?Sized` relaxations, and
    /// (trait declarations only) functional dependencies, all as one flat
    /// comma-separated list in any order. Returns empty lists when no
    /// `where` keyword is present. `parameters` is mutated in place for
    /// `?Sized` relaxations, which flip `sized` on an already-introduced
    /// parameter rather than producing a bound of their own.
    fn parse_where_clause(
        &mut self,
        parameters: &mut [TypeParameterPattern],
        allow_functional_dependencies: bool,
    ) -> Result<(Vec<TraitBound>, Vec<SubtypeBound>, Vec<FunctionalDependency>), ParseError> {
        let mut trait_bounds = Vec::new();
        let mut subtype_bounds = Vec::new();
        let mut functional_dependencies = Vec::new();
        if !self.eat(TokenKind::Where) {
            return Ok((trait_bounds, subtype_bounds, functional_dependencies));
        }
        loop {
            let start = self.position;
            if allow_functional_dependencies && self.at(TokenKind::LBrace) {
                functional_dependencies.push(self.parse_functional_dependency_set(start)?);
            } else if self.eat_operator("?") {
                let bound = self
                    .expect(TokenKind::Identifier, "expected `Sized` after `?`")?
                    .text;
                if bound != "Sized" {
                    return Err(self.error("only the implicit `Sized` bound may be relaxed"));
                }
                let name = self
                    .expect(
                        TokenKind::Identifier,
                        "expected compile-time parameter after `?Sized`",
                    )?
                    .text;
                let Some(parameter) = find_type_parameter_binding_mut(parameters, &name) else {
                    return Err(self.error(format!(
                        "`?Sized` must name an already introduced compile-time parameter; `{name}` was not found"
                    )));
                };
                if !parameter.sized {
                    return Err(self.error(format!(
                        "duplicate `?Sized` clause for compile-time parameter `{name}`"
                    )));
                }
                parameter.sized = false;
            } else {
                let first = self.expect(TokenKind::Identifier, "expected constraint")?.text;
                if allow_functional_dependencies && self.at_operator("~>") {
                    functional_dependencies
                        .push(self.parse_functional_dependency_single(start, first)?);
                } else if self.at_operator("<:") {
                    self.eat_operator("<:");
                    let supertype = self.parse_type_union()?;
                    let syntax = self.syntax(start);
                    subtype_bounds.push(SubtypeBound {
                        syntax: syntax.clone(),
                        parameter: NamedType {
                            syntax,
                            namespace: None,
                            name: first,
                        },
                        supertype,
                    });
                } else {
                    let (namespace, name) =
                        self.parse_qualified_name_from(first, "expected trait name")?;
                    if !self.starts_type_atom() {
                        return Err(self.error("expected trait bound arguments"));
                    }
                    let arguments = split_trait_arguments(self.parse_type_union()?);
                    let syntax = self.syntax(start);
                    trait_bounds.push(TraitBound {
                        syntax: syntax.clone(),
                        trait_name: NamedType {
                            syntax,
                            namespace,
                            name,
                        },
                        arguments,
                    });
                }
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        Ok((trait_bounds, subtype_bounds, functional_dependencies))
    }

    /// Parses a set-determinant functional dependency, `{A, B} ~> C`.
    fn parse_functional_dependency_set(
        &mut self,
        start: usize,
    ) -> Result<FunctionalDependency, ParseError> {
        self.expect(TokenKind::LBrace, "expected `{`")?;
        let mut determinants = Vec::new();
        if !self.at(TokenKind::RBrace) {
            loop {
                let determinant_start = self.position;
                let name = self
                    .expect(
                        TokenKind::Identifier,
                        "expected type parameter in functional dependency",
                    )?
                    .text;
                determinants.push(NamedType {
                    syntax: self.syntax(determinant_start),
                    namespace: None,
                    name,
                });
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(
            TokenKind::RBrace,
            "expected `}` after functional dependency determinants",
        )?;
        if determinants.is_empty() {
            return Err(self.error("functional dependency determinant set cannot be empty"));
        }
        if !self.eat_operator("~>") {
            return Err(self.error("expected `~>` after functional dependency determinants"));
        }
        let dependent_start = self.position;
        let dependent = self
            .expect(
                TokenKind::Identifier,
                "expected dependent type parameter after `~>`",
            )?
            .text;
        Ok(FunctionalDependency {
            syntax: self.syntax(start),
            determinants,
            dependent: NamedType {
                syntax: self.syntax(dependent_start),
                namespace: None,
                name: dependent,
            },
        })
    }

    /// Parses a single-determinant functional dependency, `A ~> C`, whose
    /// determinant identifier has already been consumed by the caller.
    fn parse_functional_dependency_single(
        &mut self,
        start: usize,
        determinant_name: String,
    ) -> Result<FunctionalDependency, ParseError> {
        let determinant = NamedType {
            syntax: self.syntax(start),
            namespace: None,
            name: determinant_name,
        };
        if !self.eat_operator("~>") {
            return Err(self.error("expected `~>` after functional dependency determinant"));
        }
        let dependent_start = self.position;
        let dependent = self
            .expect(
                TokenKind::Identifier,
                "expected dependent type parameter after `~>`",
            )?
            .text;
        Ok(FunctionalDependency {
            syntax: self.syntax(start),
            determinants: vec![determinant],
            dependent: NamedType {
                syntax: self.syntax(dependent_start),
                namespace: None,
                name: dependent,
            },
        })
    }

    fn parse_trait_implementation(
        &mut self,
        start: usize,
    ) -> Result<TraitImplementation, ParseError> {
        self.expect(TokenKind::Impl, "expected `impl`")?;
        let (type_parameters, trait_bounds, subtype_bounds) = self.parse_bracketed_generics()?;
        let trait_start = self.position;
        let first = self
            .expect(TokenKind::Identifier, "expected trait name")?
            .text;
        let (namespace, name) =
            self.parse_qualified_name_from(first, "expected trait name after namespace")?;
        let trait_name = NamedType {
            syntax: self.syntax(trait_start),
            namespace,
            name,
        };
        let arguments = split_trait_arguments(self.parse_type()?);
        self.expect(
            TokenKind::LBrace,
            "expected `{` before implementation members",
        )?;
        let mut members = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated trait implementation"));
            }
            let member_start = self.position;
            self.expect(TokenKind::Def, "expected `def` in trait implementation")?;
            let name = self
                .expect(TokenKind::Identifier, "expected implementation member name")?
                .text;
            self.expect(
                TokenKind::Equals,
                "expected `=` after implementation member name",
            )?;
            let value = self.parse_expression()?;
            members.push(ImplementationMember {
                syntax: self.syntax(member_start),
                name,
                value,
            });
            self.eat(TokenKind::Semicolon);
        }
        self.expect(
            TokenKind::RBrace,
            "expected `}` after implementation members",
        )?;
        Ok(TraitImplementation {
            syntax: self.syntax(start),
            type_parameters,
            trait_bounds,
            subtype_bounds,
            trait_name,
            arguments,
            members,
        })
    }

    /// Parses one of the item forms supported in a block expression.
    fn parse_block_item(&mut self) -> Result<Item, ParseError> {
        let item_start = self.position;
        if self.at(TokenKind::At) {
            let mut modifiers = Vec::new();
            while self.at(TokenKind::At) {
                modifiers.push(self.parse_modifier_invocation()?);
            }
            let item = Box::new(self.parse_block_item()?);
            return Ok(Item::Modified(ModifiedItem {
                syntax: self.syntax(item_start),
                modifiers,
                item,
            }));
        }
        if self.at(TokenKind::Pub) {
            return Err(self.error("public items are not supported in block expressions"));
        }
        if matches!(
            self.peek(),
            Some(
                TokenKind::Extern
                    | TokenKind::Macro
                    | TokenKind::Trait
                    | TokenKind::Impl
            )
        ) {
            return Err(self.error("unsupported item in block expression"));
        }
        self.parse_statement_with_visibility(Visibility::Private, None)
    }

    /// Parses a statement, applying `visibility` to declarations.
    fn parse_statement_with_visibility(
        &mut self,
        visibility: Visibility,
        start: Option<usize>,
    ) -> Result<Item, ParseError> {
        match self.peek() {
            Some(TokenKind::Let | TokenKind::Var) => self.parse_let_statement(visibility, start),
            Some(TokenKind::Return) if visibility == Visibility::Private => {
                self.parse_return_statement(start)
            }
            Some(TokenKind::Break) if visibility == Visibility::Private => {
                self.parse_break_statement(start)
            }
            Some(TokenKind::Continue) if visibility == Visibility::Private => {
                self.parse_continue_statement(start)
            }
            Some(TokenKind::Def) => self
                .parse_binding(visibility, start)
                .map(Item::Binding),
            Some(TokenKind::Mod) => self
                .parse_submodule(visibility, start.unwrap_or(self.position))
                .map(Item::Submodule),
            Some(TokenKind::Companion) if visibility == Visibility::Private => self
                .parse_companion(start.unwrap_or(self.position))
                .map(Item::Submodule),
            Some(TokenKind::Type) => self
                .parse_type_declaration(visibility, Visibility::Private, start.unwrap_or(self.position))
                .map(Item::TypeDeclaration),
            Some(TokenKind::Use) => self
                .parse_use_declaration(visibility, start.unwrap_or(self.position))
                .map(Item::UseDeclaration),
            _ if visibility == Visibility::Public => {
                Err(self.error("`pub` must modify a declaration"))
            }
            _ => {
                let start = self.position;
                let next_syntax_id = self.next_syntax_id;
                let target = self.parse_expression()?;
                if self.starts_declaration_macro_tail() && matches!(target, Expression::Call(_)) {
                    self.position = start;
                    self.next_syntax_id = next_syntax_id;
                    let previous_macro_punctuation = self.macro_punctuation_arguments;
                    self.macro_punctuation_arguments = true;
                    let expression = self.parse_expression();
                    self.macro_punctuation_arguments = previous_macro_punctuation;
                    return expression.map(Item::Expression);
                }
                if self.eat(TokenKind::Equals) {
                    let value = self.parse_expression()?;
                    Ok(Item::Assignment(Assignment {
                        syntax: self.syntax(start),
                        target,
                        value,
                    }))
                } else {
                    Ok(Item::Expression(target))
                }
            }
        }
    }

    fn parse_return_statement(&mut self, start: Option<usize>) -> Result<Item, ParseError> {
        let start = start.unwrap_or(self.position);
        self.expect(TokenKind::Return, "expected `return`")?;
        if self.has_newline_before_next_token() {
            return Err(self.error("expected expression after `return`"));
        }
        let value = self.parse_expression()?;
        Ok(Item::Return(ReturnStatement {
            syntax: self.syntax(start),
            value,
        }))
    }

    fn parse_break_statement(&mut self, start: Option<usize>) -> Result<Item, ParseError> {
        let start = start.unwrap_or(self.position);
        self.expect(TokenKind::Break, "expected `break`")?;
        let value = if self.has_newline_before_next_token()
            || matches!(
                self.peek(),
                None | Some(TokenKind::Semicolon | TokenKind::RBrace)
            ) {
            None
        } else {
            Some(self.parse_expression()?)
        };
        Ok(Item::Break(BreakStatement {
            syntax: self.syntax(start),
            value,
        }))
    }

    fn parse_continue_statement(&mut self, start: Option<usize>) -> Result<Item, ParseError> {
        let start = start.unwrap_or(self.position);
        self.expect(TokenKind::Continue, "expected `continue`")?;
        if !self.has_newline_before_next_token()
            && !matches!(
                self.peek(),
                None | Some(TokenKind::Semicolon | TokenKind::RBrace)
            )
        {
            return Err(self.error("expected a statement boundary after `continue`"));
        }
        Ok(Item::Continue(ContinueStatement {
            syntax: self.syntax(start),
        }))
    }

    fn parse_let_statement(
        &mut self,
        visibility: Visibility,
        start: Option<usize>,
    ) -> Result<Item, ParseError> {
        let checkpoint = self.position;
        let pattern_start = start.unwrap_or(self.position);
        let is_var = self.at(TokenKind::Var);
        if is_var {
            self.expect(TokenKind::Var, "expected `var`")?;
        } else {
            self.expect(TokenKind::Let, "expected `let`")?;
        }
        let mut pattern = self.parse_pattern()?;
        let propagating = self.eat_operator("?");
        if !matches!(pattern, Pattern::Binding(_)) {
            if visibility == Visibility::Public {
                return Err(self.error("destructuring `let` bindings cannot be public"));
            }
            if is_var {
                mark_pattern_reassignable(&mut pattern);
            }
            self.expect(
                TokenKind::Equals,
                "expected `=` after destructuring pattern",
            )?;
            let value = self.parse_expression()?;
            return Ok(Item::PatternBinding(PatternBinding {
                syntax: self.syntax(pattern_start),
                kind: if propagating {
                    PatternBindingKind::Propagating
                } else {
                    PatternBindingKind::Irrefutable
                },
                pattern,
                value,
            }));
        }
        if propagating {
            return Err(self.error("`?` requires a destructuring pattern"));
        }
        self.position = checkpoint;
        self.parse_binding(visibility, start)
            .map(Item::Binding)
    }

    /// Parses a namespace, glob, selected, or renamed `use` declaration.
    fn parse_use_declaration(
        &mut self,
        visibility: Visibility,
        start: usize,
    ) -> Result<UseDeclaration, ParseError> {
        self.expect(TokenKind::Use, "expected `use`")?;
        let mut path = vec![self.parse_quoted_identifier("expected module path after `use`")?];
        while self.at(TokenKind::Dot) && self.peek_n(1) == Some(TokenKind::Identifier) {
            self.eat(TokenKind::Dot);
            path.push(self.parse_value_name("expected module path component after `.`")?);
        }
        let kind = if self.has_newline_before_next_token() {
            if path.len() > 1 && !path.iter().all(|part| part == "super") {
                UseKind::Dotted
            } else {
                UseKind::Namespace
            }
        } else if self.at(TokenKind::Dot) && self.peek_n(1) == Some(TokenKind::Star) {
            self.eat(TokenKind::Dot);
            self.eat(TokenKind::Star);
            UseKind::Glob
        } else if self.at(TokenKind::Dot) && self.peek_n(1) == Some(TokenKind::LParen) {
            self.eat(TokenKind::Dot);
            self.eat(TokenKind::LParen);
            let mut names = Vec::new();
            if !self.at(TokenKind::RParen) {
                loop {
                    names.push(self.parse_import_name()?);
                    if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                        break;
                    }
                }
            }
            if names.is_empty() {
                return Err(self.error("selected imports require at least one item"));
            }
            self.expect(TokenKind::RParen, "expected `)` after imported items")?;
            UseKind::Selected(names)
        } else if self.eat(TokenKind::As) {
            let item = path
                .pop()
                .ok_or_else(|| self.error("expected imported item before `as`"))?;
            if path.is_empty() {
                return Err(self.error("renamed imports require a module path before the item"));
            }
            let alias = self.parse_value_name("expected import alias after `as`")?;
            UseKind::Renamed { item, alias }
        } else if self.at(TokenKind::Identifier) {
            return Err(self.error("item imports use `.` between the module path and item name"));
        } else if self.at(TokenKind::Dot) {
            return Err(self.error("expected `*` or `(` after `.` in `use` declaration"));
        } else if path.len() > 1 && !path.iter().all(|part| part == "super") {
            UseKind::Dotted
        } else {
            UseKind::Namespace
        };
        Ok(UseDeclaration {
            syntax: self.syntax(start),
            visibility,
            path,
            kind,
        })
    }

    /// Parses an `extern` block and each binding declared within it.
    fn parse_extern_block(
        &mut self,
        visibility: Visibility,
        start: usize,
    ) -> Result<ExternBlock, ParseError> {
        self.expect(TokenKind::Extern, "expected `extern`")?;
        let abi = self
            .expect(TokenKind::String, "expected ABI string after `extern`")?
            .text;
        self.expect(TokenKind::LBrace, "expected `{` after extern ABI")?;
        let mut bindings = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated extern block"));
            }
            let binding = self.parse_binding(Visibility::Private, None)?;
            if binding.mutable {
                return Err(self.error("external bindings cannot be mutable"));
            }
            if binding.reassignable {
                return Err(self.error("external bindings cannot be reassignable"));
            }
            if !binding.type_parameters.is_empty() {
                return Err(self.error("external bindings cannot have compile-time parameters"));
            }
            bindings.push(binding);
            self.eat(TokenKind::Semicolon);
        }
        self.expect(TokenKind::RBrace, "expected `}`")?;
        Ok(ExternBlock {
            syntax: self.syntax(start),
            visibility,
            abi,
            bindings,
        })
    }

    /// Parses a distinct or alias type declaration.
    fn parse_type_declaration(
        &mut self,
        visibility: Visibility,
        representation_visibility: Visibility,
        start: usize,
    ) -> Result<TypeDeclaration, ParseError> {
        self.expect(TokenKind::Type, "expected `type`")?;
        let kind = if self.eat(TokenKind::Alias) {
            TypeDeclarationKind::Alias
        } else {
            TypeDeclarationKind::Distinct
        };
        let name = self.parse_quoted_identifier("expected type name")?;
        let (mut type_parameters, default_bounds) = self.parse_juxtaposed_type_parameters()?;
        let (trait_bounds, subtype_bounds, _) =
            self.parse_where_clause(&mut type_parameters, false)?;
        let has_body = self.eat(TokenKind::Equals);
        if !has_body && kind == TypeDeclarationKind::Alias {
            return Err(self.error("expected `=` after type alias name"));
        }
        if !has_body && !type_parameters.is_empty() {
            return Err(self.error("singleton types cannot have compile-time parameters"));
        }
        let (kind, underlying) = if !has_body {
            (TypeDeclarationKind::Singleton, None)
        } else if self.eat(TokenKind::Opaque) {
            if kind == TypeDeclarationKind::Alias {
                return Err(self.error("type aliases cannot be opaque"));
            }
            (TypeDeclarationKind::Opaque, None)
        } else {
            let previous = self.newline_terminates_type;
            let previous_any = self.any_newline_terminates_type;
            self.newline_terminates_type = true;
            self.any_newline_terminates_type = true;
            let underlying = self.parse_type();
            self.newline_terminates_type = previous;
            self.any_newline_terminates_type = previous_any;
            (kind, Some(underlying?))
        };
        if representation_visibility == Visibility::Public
            && !matches!(
                kind,
                TypeDeclarationKind::Distinct | TypeDeclarationKind::Singleton
            )
        {
            return Err(self.error("`pub(repr)` requires a represented distinct type"));
        }
        Ok(TypeDeclaration {
            syntax: self.syntax(start),
            recursive_constructor: false,
            visibility,
            representation_visibility,
            kind,
            name,
            type_parameters,
            trait_bounds,
            subtype_bounds,
            default_bounds,
            underlying,
        })
    }

    /// Parses a `let` or `def` binding with optional type and value.
    fn parse_binding(
        &mut self,
        visibility: Visibility,
        start: Option<usize>,
    ) -> Result<Binding, ParseError> {
        let start = start.unwrap_or(self.position);
        let (kind, reassignable) = match self.peek() {
            Some(TokenKind::Let) => {
                self.bump_token();
                (BindingKind::Let, false)
            }
            Some(TokenKind::Var) => {
                self.bump_token();
                (BindingKind::Let, true)
            }
            Some(TokenKind::Def) => {
                self.bump_token();
                (BindingKind::Def, false)
            }
            _ => return Err(self.error("expected `let`, `var`, `def`, `type`, or `extern`")),
        };
        let mutable = self.eat(TokenKind::Mut);
        if mutable && kind != BindingKind::Let {
            return Err(self.error("`mut` is only allowed on `let` and `var` bindings"));
        }
        let name = self.parse_binding_name()?;
        let annotation = if self.eat(TokenKind::Colon) {
            Some(())
        } else {
            None
        };
        let mut type_parameters = Vec::new();
        let mut trait_bounds = Vec::new();
        let mut subtype_bounds = Vec::new();
        let annotation = if annotation.is_some() {
            let (parameters, bounds, subtypes) = self.parse_bracketed_generics()?;
            type_parameters = parameters;
            trait_bounds = bounds;
            subtype_bounds = subtypes;
            Some(self.parse_type()?)
        } else {
            None
        };
        if !type_parameters.is_empty() && kind != BindingKind::Def {
            return Err(self.error("compile-time parameters are only allowed on `def` bindings"));
        }
        let value = if self.eat(TokenKind::Equals) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(Binding {
            syntax: self.syntax(start),
            visibility,
            kind,
            mutable,
            reassignable,
            name,
            type_parameters,
            trait_bounds,
            subtype_bounds,
            annotation,
            value,
        })
    }

    /// Parses the bracketed generic-parameter form used by `let`/`def`
    /// bindings, `impl` headers, and `macro` declarations: `<param, param,
    /// ... [where constraint, ...]>`. Product patterns (`(A, B)`) are not
    /// allowed here — a flat comma list already covers declaring several
    /// parameters, so a parenthesized group would only be confusable with
    /// the comma separator. Returns empty lists when no `<` is present.
    fn parse_bracketed_generics(
        &mut self,
    ) -> Result<(Vec<TypeParameterPattern>, Vec<TraitBound>, Vec<SubtypeBound>), ParseError> {
        if !self.eat_operator("<") {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
        let mut parameters = Vec::new();
        loop {
            let param_start = self.position;
            let pattern = self.parse_type_parameter_pattern()?;
            if matches!(pattern, TypeParameterPattern::Product(_)) {
                self.position = param_start;
                return Err(self.error(
                    "product type parameters aren't allowed inside `<...>`; declare each parameter separately",
                ));
            }
            parameters.push(pattern);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        let (trait_bounds, subtype_bounds, _) = self.parse_where_clause(&mut parameters, false)?;
        if !self.eat_operator(">") {
            return Err(self.error("expected `>` to close generic parameter list"));
        }
        Ok((parameters, trait_bounds, subtype_bounds))
    }

    /// Parses the juxtaposed generic-parameter form used by `type` and
    /// `trait` declarations: bare parameters written directly after the
    /// declared name, Haskell-style, with no enclosing brackets — `Box T`,
    /// `Add Left Right Output`. Unlike the bracketed form, product patterns
    /// (`(A, B)`) and inline defaults (`(Name = Default)`) are both allowed,
    /// since there's no comma separator here to confuse them with.
    fn parse_juxtaposed_type_parameters(
        &mut self,
    ) -> Result<(Vec<TypeParameterPattern>, Vec<DefaultTypeBound>), ParseError> {
        let mut parameters = Vec::new();
        let mut defaults = Vec::new();
        while !self.has_newline_before_next_token()
            && (matches!(self.peek(), Some(TokenKind::Identifier | TokenKind::LParen))
                || (self.quote_depth > 0 && self.at(TokenKind::Dollar)))
        {
            let (pattern, default) = self.parse_type_parameter_head()?;
            parameters.push(pattern);
            if let Some(default) = default {
                defaults.push(default);
            }
        }
        Ok((parameters, defaults))
    }

    /// Parses one juxtaposed-form type parameter: a bare pattern, or
    /// `(Name = Default)` for an inline default. Disambiguated from a
    /// product pattern `(A, B, ...)` by looking two tokens ahead — a
    /// defaulted parameter is exactly `(` identifier `=`, whereas a product
    /// pattern's second token is always `,` or `)`.
    fn parse_type_parameter_head(
        &mut self,
    ) -> Result<(TypeParameterPattern, Option<DefaultTypeBound>), ParseError> {
        let start = self.position;
        if self.at(TokenKind::LParen)
            && self.peek_n(1) == Some(TokenKind::Identifier)
            && self.peek_n(2) == Some(TokenKind::Equals)
        {
            self.bump_token();
            let name = self
                .expect(TokenKind::Identifier, "expected compile-time parameter")?
                .text;
            self.expect(TokenKind::Equals, "expected `=` after compile-time parameter")?;
            let default = self.parse_type_union()?;
            self.expect(TokenKind::RParen, "expected `)` after default type")?;
            let syntax = self.syntax(start);
            let pattern = TypeParameterPattern::Binding(TypeParameterBinding {
                syntax: syntax.clone(),
                name: name.clone(),
                sized: true,
            });
            let default_bound = DefaultTypeBound {
                syntax: syntax.clone(),
                parameter: NamedType {
                    syntax,
                    namespace: None,
                    name,
                },
                default,
            };
            return Ok((pattern, Some(default_bound)));
        }
        Ok((self.parse_type_parameter_pattern()?, None))
    }

    fn parse_type_parameter_pattern(&mut self) -> Result<TypeParameterPattern, ParseError> {
        let start = self.position;
        if self.quote_depth > 0 && self.eat(TokenKind::Dollar) {
            let name = self
                .expect(TokenKind::Identifier, "expected a splice name after `$`")?
                .text;
            self.expect(
                TokenKind::Ellipsis,
                "type-parameter sequence splices require `...`",
            )?;
            return Ok(TypeParameterPattern::Splice(SpliceExpression {
                syntax: self.syntax(start),
                name,
                repeated: true,
            }));
        }
        if self.eat(TokenKind::LParen) {
            let mut elements = Vec::new();
            if !self.at(TokenKind::RParen) {
                loop {
                    elements.push(self.parse_type_parameter_pattern()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(
                TokenKind::RParen,
                "expected `)` after compile-time parameters",
            )?;
            return Ok(TypeParameterPattern::Product(TypeParameterProduct {
                syntax: self.syntax(start),
                elements,
            }));
        }
        let name = self
            .expect(TokenKind::Identifier, "expected compile-time parameter")?
            .text;
        Ok(TypeParameterPattern::Binding(TypeParameterBinding {
            syntax: self.syntax(start),
            name,
            sized: true,
        }))
    }

    /// Parses an expression, including function expressions and builtin
    /// operator expressions.
    ///
    /// Function parsing is attempted first and rewound on failure so an ordinary
    /// expression can begin with the same tokens as a parameter pattern.
    fn parse_expression(&mut self) -> Result<Expression, ParseError> {
        let checkpoint = self.position;
        let function_error = match self.parse_function_expression() {
            Ok(function) => return Ok(Expression::Function(Box::new(function))),
            Err(error) => error,
        };
        self.position = checkpoint;
        let mut expression = self.parse_or_expression()?;
        while self.eat(TokenKind::Satisfies) {
            let previous = self.newline_terminates_type;
            let previous_any = self.any_newline_terminates_type;
            self.newline_terminates_type = true;
            self.any_newline_terminates_type = true;
            let ty = self.parse_type();
            self.newline_terminates_type = previous;
            self.any_newline_terminates_type = previous_any;
            let ty = ty?;
            expression = Expression::Satisfies(Box::new(crate::SatisfiesExpression {
                syntax: self.syntax(checkpoint),
                value: Box::new(expression),
                ty,
            }));
        }
        if matches!(self.peek(), Some(TokenKind::Arrow | TokenKind::FatArrow)) {
            return Err(function_error);
        }
        Ok(expression)
    }

    /// Parses a function parameter pattern and body.
    fn parse_function_expression(&mut self) -> Result<FunctionExpression, ParseError> {
        let start = self.position;
        let pattern = self.parse_pattern()?;
        if pattern_has_mutable(&pattern) {
            return Err(self.error(
                "`mut` is not allowed on a parameter; declare it in the function's effect \
                 set, e.g. `->{mut a}`",
            ));
        }
        self.expect(TokenKind::FatArrow, "expected `=>` before function body")?;
        let body = Box::new(self.parse_expression()?);
        Ok(FunctionExpression {
            syntax: self.syntax(start),
            pattern,
            body,
        })
    }

    /// Parses either a binding pattern or a nested product pattern.
    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        let start = self.position;
        if self.eat(TokenKind::Dollar) {
            if self.quote_depth == 0 {
                return Err(self.error("splices are only allowed inside `quote`"));
            }
            let name = self
                .expect(TokenKind::Identifier, "expected a splice name after `$`")?
                .text;
            let repeated = self.eat(TokenKind::Ellipsis);
            return Ok(Pattern::Splice(SpliceExpression {
                syntax: self.syntax(start),
                name,
                repeated,
            }));
        }
        let reassignable = self.eat(TokenKind::Var);
        let mutable = self.eat(TokenKind::Mut);
        let mut pattern = if reassignable || mutable {
            self.parse_named_pattern_from(start, mutable, reassignable)?
        } else if self.eat(TokenKind::Underscore) {
            let ty = if self.eat(TokenKind::Colon) {
                self.parse_type_union()?
            } else {
                Type::Inferred(InferredType {
                    syntax: self.syntax(start),
                })
            };
            Pattern::Wildcard(WildcardPattern {
                syntax: self.syntax(start),
                ty,
            })
        } else if self.peek() == Some(TokenKind::String) {
            let literal = self.bump_token().expect("peeked string").text;
            Pattern::StringLiteral(crate::StringLiteralPattern {
                syntax: self.syntax(start),
                literal,
            })
        } else if self.eat(TokenKind::LParen) {
            let mut elements = Vec::new();
            if !self.at(TokenKind::RParen) {
                loop {
                    elements.push(self.parse_pattern()?);
                    if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RParen, "expected `)` after parameter")?;
            Pattern::Product(ProductPattern {
                syntax: self.syntax(start),
                elements,
            })
        } else {
            self.parse_named_pattern()?
        };
        if self.eat(TokenKind::At) {
            let Pattern::Binding(binding) = pattern else {
                return Err(self.error("the left side of `@` must be a binding pattern"));
            };
            let nested = Box::new(self.parse_pattern()?);
            pattern = Pattern::At(crate::AtPattern {
                syntax: self.syntax(start),
                binding: Box::new(binding),
                pattern: nested,
            });
        }
        Ok(pattern)
    }

    fn parse_named_pattern(&mut self) -> Result<Pattern, ParseError> {
        let start = self.position;
        self.parse_named_pattern_from(start, false, false)
    }

    fn parse_named_pattern_from(
        &mut self,
        start: usize,
        mutable: bool,
        reassignable: bool,
    ) -> Result<Pattern, ParseError> {
        let first = match self.peek() {
            Some(TokenKind::Identifier) => self.bump_token().expect("peeked pattern").text,
            _ => return Err(self.error("expected pattern")),
        };
        let (namespace, name) =
            self.parse_qualified_name_from(first, "expected type name after namespace")?;
        let adjacent_argument = !(self.newline_terminates_expression
            && self.has_newline_before_next_token())
            && matches!(
                self.peek(),
                Some(
                    TokenKind::Identifier
                        | TokenKind::Mut
                        | TokenKind::Var
                        | TokenKind::Underscore
                        | TokenKind::LParen
                        | TokenKind::String
                        | TokenKind::Dollar
                )
            );
        if namespace.is_some() && !adjacent_argument {
            let argument = Box::new(Pattern::Product(ProductPattern {
                syntax: self.syntax(start),
                elements: Vec::new(),
            }));
            return Ok(Pattern::Nominal(NominalPattern {
                syntax: self.syntax(start),
                namespace,
                name,
                argument,
            }));
        }
        if adjacent_argument {
            if mutable || reassignable {
                return Err(self.error("`mut` and `var` can only modify a binding pattern"));
            }
            let argument = Box::new(self.parse_pattern()?);
            return Ok(Pattern::Nominal(NominalPattern {
                syntax: self.syntax(start),
                namespace,
                name,
                argument,
            }));
        }
        let ty = if self.eat(TokenKind::Colon) {
            self.parse_type_union()?
        } else {
            Type::Inferred(InferredType {
                syntax: self.syntax(start),
            })
        };
        Ok(Pattern::Binding(BindingPattern {
            syntax: self.syntax(start),
            mutable,
            reassignable,
            name,
            resolution_name: None,
            ty,
        }))
    }

    /// Parses a type, treating function arrows as right-associative.
    fn parse_type(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        let parameter = self.parse_type_union()?;
        if self.eat(TokenKind::Arrow) {
            let resources = if self.at(TokenKind::LBrace) {
                self.parse_resource_set()?
            } else {
                ResourceSet::empty()
            };
            let result = self.parse_type()?;
            Ok(Type::Function(FunctionType {
                syntax: self.syntax(start),
                parameter: Box::new(parameter),
                resources,
                result: Box::new(result),
            }))
        } else {
            Ok(parameter)
        }
    }

    fn parse_resource_set(&mut self) -> Result<ResourceSet, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LBrace, "expected `{` before resource set")?;
        let mut resources = Vec::new();
        let mut mutations = Vec::new();
        if !self.at(TokenKind::RBrace) {
            loop {
                let entry_start = self.position;
                if self.eat(TokenKind::Mut) {
                    let target = match self.peek() {
                        Some(TokenKind::Integer) => {
                            let text = self.bump_token().expect("peeked integer").text;
                            let index = text.parse::<usize>().map_err(|_| {
                                self.error(
                                    "mutation target index must be a non-negative integer",
                                )
                            })?;
                            MutationTargetKind::Element(index)
                        }
                        Some(TokenKind::Identifier) => {
                            let name = self.bump_token().expect("peeked identifier").text;
                            MutationTargetKind::Named(name)
                        }
                        _ => MutationTargetKind::Whole,
                    };
                    mutations.push(MutationTarget {
                        syntax: self.syntax(entry_start),
                        target,
                    });
                } else {
                    resources.push(self.parse_type_union()?);
                }
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBrace, "expected `}` after resource set")?;
        Ok(ResourceSet {
            syntax: self.syntax(start),
            resources,
            mutations,
        })
    }

    /// Parses an unordered structural sum, tighter than a function arrow.
    fn parse_type_union(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        let first = self.parse_type_application()?;
        if !self.eat_operator("|") {
            return Ok(first);
        }
        let mut alternatives = vec![first, self.parse_type_application()?];
        while self.eat_operator("|") {
            alternatives.push(self.parse_type_application()?);
        }
        Ok(Type::Sum(SumType {
            syntax: self.syntax(start),
            alternatives,
        }))
    }

    fn parse_type_application(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        let mut ty = self.parse_type_postfix()?;
        while self.starts_type_atom()
            && !(self.has_newline_before_next_token()
                && (self.peek_text("resource") || self.peek_text("with")))
            && !(self.newline_terminates_type
                && self.has_newline_before_next_token()
                && (self.any_newline_terminates_type
                    || (self.peek() == Some(TokenKind::Identifier)
                        && self.peek_n(1) == Some(TokenKind::Colon))))
        {
            let argument = self.parse_type_postfix()?;
            ty = Type::Application(TypeApplication {
                syntax: self.syntax(start),
                callee: Box::new(ty),
                argument: Box::new(argument),
            });
        }
        Ok(ty)
    }

    fn parse_type_postfix(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        let mut ty = self.parse_type_atom()?;
        while self.eat(TokenKind::LBracket) {
            let count = if self.at(TokenKind::RBracket) {
                None
            } else {
                Some(
                    self.expect(TokenKind::Integer, "expected a product repetition count")?
                        .text,
                )
            };
            self.expect(TokenKind::RBracket, "expected `]` after product repetition")?;
            ty = Type::Repeated(crate::RepeatedType {
                syntax: self.syntax(start),
                element: Box::new(ty),
                count,
            });
        }
        Ok(ty)
    }

    fn starts_type_atom(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                TokenKind::Underscore
                    | TokenKind::LParen
                    | TokenKind::Identifier
                    | TokenKind::String
                    | TokenKind::Dollar
            )
        )
    }

    /// Parses a non-function type such as an inferred type, product, or name.
    fn parse_type_atom(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        if self.eat(TokenKind::Dollar) {
            if self.quote_depth == 0 {
                return Err(self.error("splices are only allowed inside `quote`"));
            }
            let name = self
                .expect(TokenKind::Identifier, "expected a splice name after `$`")?
                .text;
            if self.eat(TokenKind::Dot) {
                self.expect(TokenKind::Dollar, "expected `$` before a spliced type name")?;
                let item = self
                    .expect(TokenKind::Identifier, "expected a splice name after `$`")?
                    .text;
                return Ok(Type::Named(NamedType {
                    syntax: self.syntax(start),
                    namespace: Some(format!("${name}")),
                    name: format!("${item}"),
                }));
            }
            let repeated = self.eat(TokenKind::Ellipsis);
            return Ok(Type::Splice(SpliceExpression {
                syntax: self.syntax(start),
                name,
                repeated,
            }));
        }
        if self.eat(TokenKind::Underscore) {
            return Ok(Type::Inferred(InferredType {
                syntax: self.syntax(start),
            }));
        }
        if self.peek() == Some(TokenKind::String) {
            let literal = self.bump_token().expect("peeked string").text;
            return Ok(Type::StringLiteral(crate::StringLiteralType {
                syntax: self.syntax(start),
                literal,
            }));
        }
        if self.eat(TokenKind::LParen) {
            let mut elements = Vec::new();
            let mut variadic = false;
            if !self.at(TokenKind::RParen) {
                loop {
                    let element_start = self.position;
                    let name = if self.peek() == Some(TokenKind::Identifier)
                        && self.peek_n(1) == Some(TokenKind::Colon)
                    {
                        let name = self.bump_token().expect("peeked identifier").text;
                        self.expect(TokenKind::Colon, "expected `:` after element name")?;
                        Some(name)
                    } else {
                        None
                    };

                    if self.eat(TokenKind::Ellipsis) {
                        if self.at(TokenKind::RParen)
                            || (self.at(TokenKind::Comma)
                                && self.peek_n(1) == Some(TokenKind::RParen))
                        {
                            if name.is_some() {
                                return Err(self.error("a variadic marker cannot be named"));
                            }
                            variadic = true;
                        } else {
                            if name.is_some() {
                                return Err(self.error("a product spread cannot be named"));
                            }
                            let ty = self.parse_type()?;
                            elements.push(TypeElement {
                                syntax: self.syntax(element_start),
                                name: None,
                                ty,
                                spread: true,
                            });
                        }
                    } else {
                        let ty = self.parse_type()?;
                        elements.push(TypeElement {
                            syntax: self.syntax(element_start),
                            name,
                            ty,
                            spread: false,
                        });
                    }

                    if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RParen, "expected `)` in type")?;
            return Ok(Type::Product(ProductType {
                syntax: self.syntax(start),
                elements,
                variadic,
            }));
        }
        let first = self.expect(TokenKind::Identifier, "expected type")?.text;
        let (namespace, name) =
            self.parse_qualified_name_from(first, "expected type name after namespace")?;
        Ok(Type::Named(NamedType {
            syntax: self.syntax(start),
            namespace,
            name,
        }))
    }

    /// Comparison operators recognized at precedence 4 (non-associative),
    /// paired with the trait/method they desugar to.
    const COMPARISON_OPERATORS: [(&'static str, &'static str, &'static str); 6] = [
        ("==", "Eq", "equal"),
        ("!=", "Eq", "not_equal"),
        ("<=", "PartialOrd", "le"),
        (">=", "PartialOrd", "ge"),
        ("<", "PartialOrd", "lt"),
        (">", "PartialOrd", "gt"),
    ];

    /// Parses `||` (precedence 1, left-associative), producing a
    /// `LogicalExpression` rather than desugaring to a call: `||` is not
    /// backed by a trait and cannot be overloaded, and the right operand must
    /// be evaluated lazily.
    fn parse_or_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_and_expression()?;
        loop {
            if self.newline_terminates_expression && self.has_newline_before_next_token() {
                break;
            }
            let operator_start = self.position;
            if !self.eat_operator("||") {
                break;
            }
            let bool_type = self.bool_type(operator_start);
            let right = self.parse_and_expression()?;
            expression = Expression::Logical(LogicalExpression {
                syntax: self.syntax(start),
                operator: LogicalOperator::Or,
                left: Box::new(expression),
                right: Box::new(right),
                bool_type,
            });
        }
        Ok(expression)
    }

    /// Parses `&&` (precedence 2, left-associative), producing a
    /// `LogicalExpression` rather than desugaring to a call: `&&` is not
    /// backed by a trait and cannot be overloaded, and the right operand must
    /// be evaluated lazily.
    fn parse_and_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_range_expression()?;
        loop {
            if self.newline_terminates_expression && self.has_newline_before_next_token() {
                break;
            }
            let operator_start = self.position;
            if !self.eat_operator("&&") {
                break;
            }
            let bool_type = self.bool_type(operator_start);
            let right = self.parse_range_expression()?;
            expression = Expression::Logical(LogicalExpression {
                syntax: self.syntax(start),
                operator: LogicalOperator::And,
                left: Box::new(expression),
                right: Box::new(right),
                bool_type,
            });
        }
        Ok(expression)
    }

    /// Builds a reference to the prelude `Bool` type, tied to the operator's
    /// own span. Must be called immediately after consuming the operator
    /// token and before parsing the right operand; see
    /// `apply_trait_operator`.
    fn bool_type(&mut self, start: usize) -> Type {
        Type::Named(NamedType {
            syntax: self.syntax(start),
            namespace: None,
            name: "Bool".to_owned(),
        })
    }

    /// Parses a `..`/`..=` range expression (precedence 3, non-associative),
    /// desugaring directly to a call to the prelude's `range`/`range_inclusive`
    /// functions.
    fn parse_range_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let left = self.parse_comparison_expression()?;
        if self.newline_terminates_expression && self.has_newline_before_next_token() {
            return Ok(left);
        }
        let operator_start = self.position;
        let function_name = if self.eat_operator("..=") {
            "range_inclusive"
        } else if self.eat_operator("..") {
            "range"
        } else {
            return Ok(left);
        };
        let applied = self.apply_prelude_operator(start, operator_start, function_name, left);
        let right = self.parse_comparison_expression()?;
        let expression = Expression::Call(CallExpression {
            syntax: self.syntax(start),
            callee: Box::new(applied),
            argument: Box::new(right),
        });
        if !(self.newline_terminates_expression && self.has_newline_before_next_token())
            && (self.at_operator("..=") || self.at_operator(".."))
        {
            return Err(self.error(
                "range operators cannot be chained; parenthesize to disambiguate",
            ));
        }
        Ok(expression)
    }

    /// Parses `==`/`!=`/`<`/`<=`/`>`/`>=` (precedence 4, non-associative),
    /// desugaring directly to the corresponding `Eq`/`PartialOrd` trait call.
    fn parse_comparison_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let left = self.parse_additive_expression()?;
        if self.newline_terminates_expression && self.has_newline_before_next_token() {
            return Ok(left);
        }
        let operator_start = self.position;
        let Some((trait_name, method_name)) = self.eat_comparison_operator() else {
            return Ok(left);
        };
        let applied =
            self.apply_trait_operator(start, operator_start, trait_name, method_name, left);
        let right = self.parse_additive_expression()?;
        let expression = Expression::Call(CallExpression {
            syntax: self.syntax(start),
            callee: Box::new(applied),
            argument: Box::new(right),
        });
        if !(self.newline_terminates_expression && self.has_newline_before_next_token())
            && self.at_comparison_operator()
        {
            return Err(self.error(
                "comparison operators cannot be chained; parenthesize to disambiguate",
            ));
        }
        Ok(expression)
    }

    /// Parses `+`/`-` (precedence 6, left-associative), desugaring each
    /// application directly to the corresponding `Add`/`Subtract` trait call.
    fn parse_additive_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_multiplicative_expression()?;
        loop {
            if self.newline_terminates_expression && self.has_newline_before_next_token() {
                break;
            }
            let operator_start = self.position;
            let operator = if self.eat(TokenKind::Plus) {
                Some(("Add", "add"))
            } else if self.eat(TokenKind::Minus) {
                Some(("Subtract", "subtract"))
            } else {
                None
            };
            let Some((trait_name, method_name)) = operator else {
                break;
            };
            let applied = self.apply_trait_operator(
                start,
                operator_start,
                trait_name,
                method_name,
                expression,
            );
            let right = self.parse_multiplicative_expression()?;
            expression = Expression::Call(CallExpression {
                syntax: self.syntax(start),
                callee: Box::new(applied),
                argument: Box::new(right),
            });
        }
        Ok(expression)
    }

    /// Parses `*`/`/` (precedence 7, left-associative), desugaring each
    /// application directly to the corresponding `Multiply`/`Divide` trait call.
    fn parse_multiplicative_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_call_expression()?;
        loop {
            if self.newline_terminates_expression && self.has_newline_before_next_token() {
                break;
            }
            let operator_start = self.position;
            let operator = if self.eat(TokenKind::Star) {
                Some(("Multiply", "multiply"))
            } else if self.eat(TokenKind::Slash) {
                Some(("Divide", "divide"))
            } else {
                None
            };
            let Some((trait_name, method_name)) = operator else {
                break;
            };
            let applied = self.apply_trait_operator(
                start,
                operator_start,
                trait_name,
                method_name,
                expression,
            );
            let right = self.parse_call_expression()?;
            expression = Expression::Call(CallExpression {
                syntax: self.syntax(start),
                callee: Box::new(applied),
                argument: Box::new(right),
            });
        }
        Ok(expression)
    }

    fn eat_comparison_operator(&mut self) -> Option<(&'static str, &'static str)> {
        for (text, trait_name, method_name) in Self::COMPARISON_OPERATORS {
            if self.eat_operator(text) {
                return Some((trait_name, method_name));
            }
        }
        None
    }

    fn at_comparison_operator(&self) -> bool {
        Self::COMPARISON_OPERATORS
            .iter()
            .any(|(text, _, _)| self.at_operator(text))
    }

    /// Builds `Trait.method left` using the operator token's own span for the
    /// synthesized `Trait`/`method` names. Must be called immediately after
    /// consuming the operator token and before parsing the right operand, so
    /// the synthesized spans stay tied to the operator instead of growing to
    /// include the right-hand side.
    fn apply_trait_operator(
        &mut self,
        start: usize,
        operator_start: usize,
        trait_name: &str,
        method_name: &str,
        left: Expression,
    ) -> Expression {
        let name = Expression::Name(NameExpression {
            syntax: self.syntax(operator_start),
            name: trait_name.to_owned(),
        });
        let access = Expression::Access(AccessExpression {
            syntax: self.syntax(operator_start),
            value: Box::new(name),
            accessor: Accessor::Name(method_name.to_owned()),
        });
        Expression::Call(CallExpression {
            syntax: self.syntax(start),
            callee: Box::new(access),
            argument: Box::new(left),
        })
    }

    /// Builds `function left` for a builtin operator that desugars to an
    /// ordinary prelude function rather than a trait method (`..`/`..=`). See
    /// `apply_trait_operator` for the span-timing requirement.
    fn apply_prelude_operator(
        &mut self,
        start: usize,
        operator_start: usize,
        function_name: &str,
        left: Expression,
    ) -> Expression {
        let name = Expression::Name(NameExpression {
            syntax: self.syntax(operator_start),
            name: function_name.to_owned(),
        });
        Expression::Call(CallExpression {
            syntax: self.syntax(start),
            callee: Box::new(name),
            argument: Box::new(left),
        })
    }

    /// Parses juxtaposition-based function calls.
    fn parse_call_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_access_expression()?;
        while self.starts_atom() {
            let checkpoint = self.position;
            let next_syntax_id = self.next_syntax_id;
            let argument = match self.parse_access_expression() {
                Ok(argument) => argument,
                Err(error)
                    if self
                        .tokens
                        .get(self.next_non_trivia(checkpoint))
                        .is_some_and(|token| {
                            matches!(
                                token.kind,
                                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace
                            )
                        }) =>
                {
                    self.position = checkpoint;
                    self.next_syntax_id = next_syntax_id;
                    self.parse_syntax_argument().map_err(|_| error)?
                }
                Err(error) => return Err(error),
            };
            expression = Expression::Call(CallExpression {
                syntax: self.syntax(start),
                callee: Box::new(expression),
                argument: Box::new(argument),
            });
        }
        Ok(expression)
    }

    /// Captures one balanced delimited macro argument whose contents are
    /// interpreted during macro matching.
    fn parse_syntax_argument(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let close = match self.peek() {
            Some(TokenKind::LParen) => TokenKind::RParen,
            Some(TokenKind::LBracket) => TokenKind::RBracket,
            Some(TokenKind::LBrace) => TokenKind::RBrace,
            _ => return Err(self.error("expected a delimited syntax argument")),
        };
        self.bump_token();
        let mut delimiters = vec![close];
        while let Some(kind) = self.peek() {
            match kind {
                TokenKind::LParen => {
                    self.bump_token();
                    delimiters.push(TokenKind::RParen);
                }
                TokenKind::LBracket => {
                    self.bump_token();
                    delimiters.push(TokenKind::RBracket);
                }
                TokenKind::LBrace => {
                    self.bump_token();
                    delimiters.push(TokenKind::RBrace);
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    if delimiters.last().copied() != Some(kind) {
                        return Err(self.error("mismatched delimiter in syntax argument"));
                    }
                    self.bump_token();
                    delimiters.pop();
                    if delimiters.is_empty() {
                        return Ok(Expression::SyntaxArgument(SyntaxArgumentExpression {
                            syntax: self.syntax(start),
                        }));
                    }
                }
                _ => {
                    self.bump_token();
                }
            }
        }
        Err(self.error("expected closing delimiter after syntax argument"))
    }

    /// Parses chained named or positional product access.
    fn parse_access_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_atom()?;
        loop {
            if self.eat(TokenKind::Dot) {
                let accessor = match self.peek() {
                    Some(TokenKind::Identifier) => {
                        Accessor::Name(self.bump_token().expect("peeked name").text)
                    }
                    Some(TokenKind::Integer) => {
                        Accessor::Index(self.bump_token().expect("peeked index").text)
                    }
                    _ => {
                        return Err(
                            self.error("expected a product element name or index after `.`")
                        );
                    }
                };
                expression = Expression::Access(AccessExpression {
                    syntax: self.syntax(start),
                    value: Box::new(expression),
                    accessor,
                });
            } else if self.eat_operator("^") {
                let method = self
                    .expect(TokenKind::Identifier, "expected companion method name after `^`")?
                    .text;
                let receiver = expression;
                let selector = Expression::Access(AccessExpression {
                    syntax: self.syntax(self.position.saturating_sub(2)),
                    value: Box::new(receiver.clone()),
                    accessor: Accessor::Method(method),
                });
                expression = Expression::Call(CallExpression {
                    syntax: self.syntax(start),
                    callee: Box::new(selector),
                    argument: Box::new(receiver),
                });
            } else if !self.has_trivia_before_next_token() && self.eat(TokenKind::LBracket) {
                let index = self.parse_expression()?;
                self.expect(TokenKind::RBracket, "expected `]` after product index")?;
                expression = Expression::Index(crate::IndexExpression {
                    syntax: self.syntax(start),
                    value: Box::new(expression),
                    index: Box::new(index),
                });
            } else {
                break;
            }
        }
        Ok(expression)
    }

    /// Parses a primary expression without call, access, or binary operators.
    fn parse_atom(&mut self) -> Result<Expression, ParseError> {
        if let Some(path) = self.quote_expression_start() {
            return self
                .parse_quote_expression(path)
                .map(Expression::Quote);
        }
        if self.peek_text("resource") && self.is_resource_expression_start() {
            return self
                .parse_resource_expression()
                .map(|value| Expression::Resource(Box::new(value)));
        }
        if self.peek_text("with") && self.is_with_resource_expression_start() {
            return self
                .parse_with_resource_expression()
                .map(|value| Expression::With(Box::new(value)));
        }
        match self.peek() {
            Some(TokenKind::Match) => self.parse_match_expression().map(Expression::Match),
            Some(TokenKind::Loop) => self.parse_loop_expression().map(Expression::Loop),
            Some(TokenKind::Pub) => {
                let start = self.position;
                self.expect(TokenKind::Pub, "expected `pub`")?;
                let kind = if self.eat(TokenKind::LParen) {
                    let repr = self.expect(
                        TokenKind::Identifier,
                        "expected `repr` in visibility argument",
                    )?;
                    if repr.text != "repr" {
                        return Err(self.error("expected `repr` in visibility argument"));
                    }
                    self.expect(TokenKind::RParen, "expected `)` after `repr`")?;
                    VisibilityKind::PublicRepr
                } else {
                    VisibilityKind::Public
                };
                Ok(Expression::VisibilityArgument(VisibilitySyntax {
                    syntax: self.syntax(start),
                    kind,
                }))
            }
            Some(TokenKind::LBrace) => self.parse_block_expression().map(Expression::Block),
            Some(TokenKind::LBracket) => self.parse_syntax_argument(),
            Some(TokenKind::LParen) => self.parse_product_expression().map(Expression::Product),
            Some(TokenKind::Identifier | TokenKind::Underscore) => {
                let start = self.position;
                let name = self.bump_token().expect("peeked name").text;
                Ok(Expression::Name(NameExpression {
                    syntax: self.syntax(start),
                    name,
                }))
            }
            Some(TokenKind::String) => {
                let start = self.position;
                let literal = self.bump_token().expect("peeked string").text;
                Ok(Expression::String(StringExpression {
                    syntax: self.syntax(start),
                    literal,
                }))
            }
            Some(TokenKind::Integer) => {
                let start = self.position;
                let literal = self.bump_token().expect("peeked integer").text;
                Ok(Expression::Integer(IntegerExpression {
                    syntax: self.syntax(start),
                    literal,
                }))
            }
            Some(TokenKind::Float) => {
                let start = self.position;
                let literal = self.bump_token().expect("peeked float").text;
                Ok(Expression::Float(FloatExpression {
                    syntax: self.syntax(start),
                    literal,
                }))
            }
            Some(TokenKind::Dollar) if self.quote_depth > 0 => {
                let start = self.position;
                self.expect(TokenKind::Dollar, "expected `$`")?;
                let name = self
                    .expect(TokenKind::Identifier, "expected a splice name after `$`")?
                    .text;
                let repeated = self.eat(TokenKind::Ellipsis);
                Ok(Expression::Splice(SpliceExpression {
                    syntax: self.syntax(start),
                    name,
                    repeated,
                }))
            }
            Some(TokenKind::Dollar) => Err(self.error("splices are only allowed inside `quote`")),
            Some(TokenKind::Equals | TokenKind::FatArrow) if self.macro_punctuation_arguments => {
                let start = self.position;
                self.bump_token();
                Ok(Expression::SyntaxArgument(SyntaxArgumentExpression {
                    syntax: self.syntax(start),
                }))
            }
            _ => Err(self.error("expected expression")),
        }
    }

    fn quote_expression_start(&self) -> Option<Vec<String>> {
        let mut position = self.next_non_trivia(self.position);
        let mut path = vec![self.tokens.get(position)?
            .kind.eq(&TokenKind::Identifier)
            .then(|| self.tokens[position].text.clone())?];
        position += 1;
        loop {
            position = self.next_non_trivia(position);
            if self.tokens.get(position)?.kind != TokenKind::Dot {
                break;
            }
            position = self.next_non_trivia(position + 1);
            let token = self.tokens.get(position)?;
            if token.kind != TokenKind::Identifier {
                return None;
            }
            path.push(token.text.clone());
            position += 1;
        }
        position = self.next_non_trivia(position);
        (matches!(path.last()?.as_str(), "quote" | "parse_quote")
            && self.tokens.get(position)?.kind == TokenKind::LBrace)
            .then_some(path)
    }

    fn parse_quote_expression(&mut self, path: Vec<String>) -> Result<QuoteExpression, ParseError> {
        let start = self.position;
        for _ in 0..path.len() - 1 {
            self.expect(TokenKind::Identifier, "expected quotation macro path")?;
            self.expect(TokenKind::Dot, "expected `.` in quotation macro path")?;
        }
        let quote = self.expect(TokenKind::Identifier, "expected `quote` or `parse_quote`")?;
        debug_assert!(quote.text == "quote" || quote.text == "parse_quote");
        let kind = if quote.text == "parse_quote" {
            QuoteKind::ParseQuote
        } else {
            QuoteKind::Quote
        };
        self.expect(TokenKind::LBrace, "expected `{` after `quote`")?;
        self.quote_depth += 1;
        let previous = self.brace_terminates_expression;
        self.brace_terminates_expression = true;
        let template_start = self.position;
        let next_syntax_id = self.next_syntax_id;
        let expression = self.parse_expression();
        let template = match expression {
            Ok(expression) if self.at(TokenKind::RBrace) => {
                Ok(crate::QuoteTemplate::Expression(Box::new(expression)))
            }
            _ => {
                self.position = template_start;
                self.next_syntax_id = next_syntax_id;
                let mut items = Vec::new();
                while !self.at(TokenKind::RBrace) && self.peek().is_some() {
                    match self.parse_item() {
                        Ok(item) => {
                            items.push(item);
                            self.eat(TokenKind::Semicolon);
                        }
                        Err(_) => {
                            items.clear();
                            break;
                        }
                    }
                }
                if self.at(TokenKind::RBrace) && !items.is_empty() {
                    Ok(if items.len() == 1 {
                        crate::QuoteTemplate::Item(Box::new(items.remove(0)))
                    } else {
                        crate::QuoteTemplate::Items(items)
                    })
                } else {
                    self.position = template_start;
                    self.next_syntax_id = next_syntax_id;
                    let mut depth = 0usize;
                    while self.peek().is_some() {
                        if self.at(TokenKind::RBrace) && depth == 0 {
                            break;
                        }
                        let token = self.bump_token().expect("peeked quote token");
                        match token.kind {
                            TokenKind::LBrace => depth += 1,
                            TokenKind::RBrace => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                    Ok(crate::QuoteTemplate::Raw)
                }
            }
        };
        self.brace_terminates_expression = previous;
        self.quote_depth -= 1;
        let template = template?;
        let contents = self.syntax(template_start);
        self.expect(TokenKind::RBrace, "expected `}` after quoted syntax")?;
        Ok(QuoteExpression {
            syntax: self.syntax(start),
            kind,
            path,
            contents,
            template,
        })
    }

    fn parse_quoted_identifier(&mut self, message: &'static str) -> Result<String, ParseError> {
        if self.quote_depth > 0 && self.eat(TokenKind::Dollar) {
            return self
                .expect(
                    TokenKind::Identifier,
                    "expected an identifier splice after `$`",
                )
                .map(|token| format!("${}", token.text));
        }
        self.expect(TokenKind::Identifier, message)
            .map(|token| token.text)
    }

    fn parse_match_expression(&mut self) -> Result<MatchExpression, ParseError> {
        let start = self.position;
        self.expect(TokenKind::Match, "expected `match`")?;
        let previous = self.brace_terminates_expression;
        self.brace_terminates_expression = true;
        let subject = self.parse_expression();
        self.brace_terminates_expression = previous;
        let subject = Box::new(subject?);
        self.expect(TokenKind::LBrace, "expected `{` before match arms")?;
        let mut arms = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated match expression"));
            }
            let arm_start = self.position;
            let pattern = self.parse_pattern()?;
            self.expect(TokenKind::FatArrow, "expected `=>` after match pattern")?;
            let body = self.parse_expression()?;
            arms.push(MatchArm {
                syntax: self.syntax(arm_start),
                pattern,
                body,
            });
            if !self.eat(TokenKind::Comma) && !self.at(TokenKind::RBrace) {
                return Err(self.error("expected `,` after match arm"));
            }
        }
        self.expect(TokenKind::RBrace, "expected `}` after match arms")?;
        if arms.is_empty() {
            return Err(self.error("a match expression requires at least one arm"));
        }
        Ok(MatchExpression {
            syntax: self.syntax(start),
            subject,
            arms,
        })
    }

    fn parse_loop_expression(&mut self) -> Result<LoopExpression, ParseError> {
        let start = self.position;
        self.expect(TokenKind::Loop, "expected `loop`")?;
        let body = self.parse_block_expression()?;
        Ok(LoopExpression {
            syntax: self.syntax(start),
            body,
        })
    }

    fn parse_resource_expression(&mut self) -> Result<ResourceExpression, ParseError> {
        let start = self.position;
        let keyword = self.expect(TokenKind::Identifier, "expected `resource`")?;
        if keyword.text != "resource" {
            return Err(self.error("expected `resource`"));
        }
        let resource = self.parse_type_union()?;
        Ok(ResourceExpression {
            syntax: self.syntax(start),
            resource,
        })
    }

    fn is_resource_expression_start(&self) -> bool {
        self.peek_n(1).is_some_and(is_type_atom_start)
    }

    fn is_with_resource_expression_start(&self) -> bool {
        let mut candidate = self.clone();
        candidate.bump_token();
        candidate.parse_type_union().is_ok() && candidate.at(TokenKind::Equals)
    }

    fn parse_with_resource_expression(&mut self) -> Result<WithResourceExpression, ParseError> {
        let start = self.position;
        let keyword = self.expect(TokenKind::Identifier, "expected `with`")?;
        if keyword.text != "with" {
            return Err(self.error("expected `with`"));
        }
        let resource = self.parse_type_union()?;
        self.expect(TokenKind::Equals, "expected `=` after resource type")?;
        let previous = self.brace_terminates_expression;
        self.brace_terminates_expression = true;
        let value = self.parse_expression();
        self.brace_terminates_expression = previous;
        let value = Box::new(value?);
        let body = self.parse_block_expression()?;
        Ok(WithResourceExpression {
            syntax: self.syntax(start),
            resource,
            value,
            body,
        })
    }

    /// Parses a brace-delimited sequence of items.
    fn parse_block_expression(&mut self) -> Result<BlockExpression, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LBrace, "expected `{`")?;
        let mut items = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated block expression"));
            }
            items.push(self.parse_block_item()?);
            self.eat(TokenKind::Semicolon);
        }
        self.expect(TokenKind::RBrace, "expected `}`")?;
        Ok(BlockExpression {
            syntax: self.syntax(start),
            items,
        })
    }

    /// Parses a parenthesized product expression with optional element names.
    fn parse_product_expression(&mut self) -> Result<ProductExpression, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LParen, "expected `(`")?;
        let previous_brace_termination = self.brace_terminates_expression;
        self.brace_terminates_expression = false;
        let mut elements = Vec::new();
        if !self.at(TokenKind::RParen) {
            loop {
                let element_start = self.position;
                let name = if self.peek() == Some(TokenKind::Identifier)
                    && self.peek_n(1) == Some(TokenKind::Colon)
                {
                    let name = self.bump_token().expect("peeked element name").text;
                    self.expect(TokenKind::Colon, "expected `:` after element name")?;
                    Some(name)
                } else {
                    None
                };
                let spread = self.eat(TokenKind::Ellipsis);
                let named_spread = spread && self.eat(TokenKind::Equals);
                if spread && name.is_some() {
                    return Err(self.error("a product value spread cannot be named"));
                }
                let value = self.parse_expression()?;
                elements.push(ProductElement {
                    syntax: self.syntax(element_start),
                    name,
                    value,
                    spread,
                    named_spread,
                });
                if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                    break;
                }
            }
        }
        let close = self.expect(TokenKind::RParen, "expected `)` after product");
        self.brace_terminates_expression = previous_brace_termination;
        close?;
        Ok(ProductExpression {
            syntax: self.syntax(start),
            elements,
        })
    }

    /// Returns whether the next token can begin an atom in the current context.
    fn starts_atom(&self) -> bool {
        if self.newline_terminates_expression && self.has_newline_before_next_token() {
            return false;
        }
        if self.brace_terminates_expression && self.peek() == Some(TokenKind::LBrace) {
            return false;
        }
        matches!(
            self.peek(),
            Some(
                TokenKind::LBrace
                    | TokenKind::LParen
                    | TokenKind::LBracket
                    | TokenKind::Match
                    | TokenKind::Loop
                    | TokenKind::Pub
                    | TokenKind::Identifier
                    | TokenKind::Underscore
                    | TokenKind::Dollar
                    | TokenKind::String
                    | TokenKind::Integer
                    | TokenKind::Float
            )
        ) || self.macro_punctuation_arguments
            && matches!(self.peek(), Some(TokenKind::Equals | TokenKind::FatArrow))
    }

    fn starts_declaration_macro_tail(&self) -> bool {
        if self.peek() != Some(TokenKind::Equals) {
            return false;
        }
        match self.peek_n(1) {
            Some(TokenKind::LBrace) => true,
            Some(TokenKind::Identifier) => self.peek_n(2) == Some(TokenKind::FatArrow),
            Some(TokenKind::LParen) => {
                let mut depth = 0usize;
                let mut offset = 1usize;
                loop {
                    match self.peek_n(offset) {
                        Some(TokenKind::LParen) => depth += 1,
                        Some(TokenKind::RParen) => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                return self.peek_n(offset + 1) == Some(TokenKind::FatArrow);
                            }
                        }
                        None => return false,
                        _ => {}
                    }
                    offset += 1;
                }
            }
            _ => false,
        }
    }

    fn parse_value_name(&mut self, message: &'static str) -> Result<String, ParseError> {
        if self.peek() == Some(TokenKind::Identifier) {
            Ok(self.bump_token().expect("peeked value name").text)
        } else {
            Err(self.error(message))
        }
    }

    fn parse_qualified_name_from(
        &mut self,
        first: String,
        message: &'static str,
    ) -> Result<(Option<String>, String), ParseError> {
        let mut parts = vec![first];
        while self.peek() == Some(TokenKind::Dot) && self.peek_n(1) == Some(TokenKind::Identifier) {
            self.eat(TokenKind::Dot);
            parts.push(self.expect(TokenKind::Identifier, message)?.text);
        }
        let name = parts.pop().expect("qualified name has a first component");
        Ok(((!parts.is_empty()).then(|| parts.join(".")), name))
    }

    fn parse_binding_name(&mut self) -> Result<String, ParseError> {
        self.parse_value_name("expected binding name")
    }

    fn parse_import_name(&mut self) -> Result<String, ParseError> {
        self.parse_value_name("expected imported item name")
    }

    /// Returns the next non-trivia token kind without consuming it.
    fn peek(&self) -> Option<TokenKind> {
        self.peek_n(0)
    }

    /// Returns the `n`th non-trivia token kind without consuming it.
    fn peek_n(&self, n: usize) -> Option<TokenKind> {
        let mut position = self.position;
        for index in 0..=n {
            position = self.next_non_trivia(position);
            let token = self.tokens.get(position)?;
            if index == n {
                return Some(token.kind);
            }
            position += 1;
        }
        None
    }

    fn peek_text(&self, expected: &str) -> bool {
        self.tokens
            .get(self.next_non_trivia(self.position))
            .is_some_and(|token| token.text == expected)
    }

    /// Returns whether the next non-trivia token has `kind`.
    fn at(&self, kind: TokenKind) -> bool {
        self.peek() == Some(kind)
    }

    /// Consumes the next non-trivia token when it has `kind`.
    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.bump_token();
            true
        } else {
            false
        }
    }

    fn eat_operator(&mut self, expected: &str) -> bool {
        let position = self.next_non_trivia(self.position);
        if self
            .tokens
            .get(position)
            .is_some_and(|token| token.kind == TokenKind::Operator && token.text == expected)
        {
            self.position = position + 1;
            true
        } else {
            false
        }
    }

    /// Returns whether the next non-trivia token is an exact-text operator
    /// token, without consuming it.
    fn at_operator(&self, expected: &str) -> bool {
        self.tokens
            .get(self.next_non_trivia(self.position))
            .is_some_and(|token| token.kind == TokenKind::Operator && token.text == expected)
    }

    /// Consumes a required token or reports `message` at the cursor.
    fn expect(
        &mut self,
        kind: TokenKind,
        message: &'static str,
    ) -> Result<SyntaxToken, ParseError> {
        if self.at(kind) {
            Ok(self.bump_token().expect("peeked token"))
        } else {
            Err(self.error(message))
        }
    }

    /// Consumes and returns the next non-trivia token.
    fn bump_token(&mut self) -> Option<SyntaxToken> {
        self.position = self.next_non_trivia(self.position);
        let token = self.tokens.get(self.position)?.clone();
        self.position += 1;
        Some(token)
    }

    /// Advances an arbitrary token index past trivia.
    fn next_non_trivia(&self, mut position: usize) -> usize {
        while self
            .tokens
            .get(position)
            .is_some_and(|token| token.kind.is_trivia())
        {
            position += 1;
        }
        position
    }

    /// Returns whether skipped trivia before the next token contains a newline.
    fn has_newline_before_next_token(&self) -> bool {
        self.tokens[self.position..self.next_non_trivia(self.position)]
            .iter()
            .any(|token| token.kind == TokenKind::Newline)
    }

    fn has_trivia_before_next_token(&self) -> bool {
        self.position < self.next_non_trivia(self.position)
    }

    /// Builds syntax metadata for tokens consumed since `start`.
    fn syntax(&mut self, start: usize) -> Syntax {
        let tokens = &self.tokens[start..self.position];
        let span_start = tokens
            .first()
            .map_or(self.source_len, |token| token.span.start);
        let span_end = tokens.last().map_or(span_start, |token| token.span.end);
        let diagnostic_start = tokens
            .iter()
            .find(|token| !token.kind.is_trivia())
            .map_or(span_start, |token| token.span.start);
        let id = SyntaxId(self.next_syntax_id);
        self.next_syntax_id += 1;
        Syntax {
            id,
            span: Span::User {
                source: self.source_name.clone(),
                range: span_start..span_end,
                location: Some(self.location(diagnostic_start)),
            },
            tokens: Arc::clone(&self.tokens),
            token_range: start..self.position,
            definition_module: None,
            expansion_mark: None,
            identifier_origins: Vec::new(),
        }
    }

    /// Creates a parse error at the next non-trivia token.
    fn error(&self, message: impl Into<String>) -> ParseError {
        let position = self.next_non_trivia(self.position);
        let offset = self
            .tokens
            .get(position)
            .map_or(self.source_len, |token| token.span.start);
        ParseError {
            offset,
            location: self.location(offset),
            message: message.into(),
        }
    }

    fn location(&self, offset: usize) -> SourceLocation {
        let line = self.line_starts.partition_point(|start| *start <= offset);
        let line_start = self.line_starts[line - 1];
        SourceLocation {
            line,
            column: self.source[line_start..offset].chars().count() + 1,
        }
    }
}

fn line_starts(source: &str) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut starts = vec![0];
    let mut offset = 0;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\r' if bytes.get(offset + 1) == Some(&b'\n') => {
                offset += 2;
                starts.push(offset);
            }
            b'\r' | b'\n' => {
                offset += 1;
                starts.push(offset);
            }
            _ => offset += 1,
        }
    }
    starts
}

fn find_type_parameter_binding_mut<'a>(
    parameters: &'a mut [TypeParameterPattern],
    name: &str,
) -> Option<&'a mut TypeParameterBinding> {
    for parameter in parameters {
        match parameter {
            TypeParameterPattern::Binding(binding) if binding.name == name => {
                return Some(binding);
            }
            TypeParameterPattern::Product(product) => {
                if let Some(binding) = find_type_parameter_binding_mut(&mut product.elements, name)
                {
                    return Some(binding);
                }
            }
            TypeParameterPattern::Binding(_) => {}
            TypeParameterPattern::Splice(_) => {}
        }
    }
    None
}

/// Splits the outer application spine of a trait use into its compile-time
/// arguments. Parenthesized types remain product nodes, so they continue to
/// group a complex type into one argument just as they do for type application.
fn split_trait_arguments(mut ty: Type) -> Vec<Type> {
    let mut arguments = Vec::new();
    while let Type::Application(application) = ty {
        arguments.push(*application.argument);
        ty = *application.callee;
    }
    arguments.push(ty);
    arguments.reverse();
    arguments
}

fn companion_target_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Named(named) => Some(named.name.clone()),
        Type::Application(application) => companion_target_name(&application.callee),
        Type::Splice(splice) => Some(format!("${}", splice.name)),
        _ => None,
    }
}

/// Marks every binding pattern reachable from `pattern` as reassignable, used
/// to propagate a leading `var` across a destructuring pattern.
fn mark_pattern_reassignable(pattern: &mut Pattern) {
    match pattern {
        Pattern::Binding(binding) => binding.reassignable = true,
        Pattern::At(at) => {
            at.binding.reassignable = true;
            mark_pattern_reassignable(&mut at.pattern);
        }
        Pattern::Product(product) => {
            for element in &mut product.elements {
                mark_pattern_reassignable(element);
            }
        }
        Pattern::Nominal(nominal) => mark_pattern_reassignable(&mut nominal.argument),
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
    }
}

/// Returns whether `pattern` marks any binding `mut`. Function parameter
/// patterns reject this: parameter mutability is declared in the function's
/// effect set instead.
fn pattern_has_mutable(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Binding(binding) => binding.mutable,
        Pattern::At(at) => at.binding.mutable || pattern_has_mutable(&at.pattern),
        Pattern::Product(product) => product.elements.iter().any(pattern_has_mutable),
        Pattern::Nominal(nominal) => pattern_has_mutable(&nominal.argument),
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => false,
    }
}

fn is_type_atom_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Underscore | TokenKind::LParen | TokenKind::Identifier
    )
}
