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

struct Grammar {
    tokens: Arc<[SyntaxToken]>,
    position: usize,
    source: Arc<str>,
    source_len: usize,
    line_starts: Vec<usize>,
    next_syntax_id: usize,
    newline_terminates_expression: bool,
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
            source_name,
        }
    }

    /// Parses all top-level items and returns the completed module.
    fn parse_source_file(mut self) -> Result<Module, ParseError> {
        let start = self.position;
        let mut items = Vec::new();
        while self.peek().is_some() {
            items.push(self.parse_item()?);
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
        let visibility = if self.eat(TokenKind::Pub) {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let item = match self.peek() {
            Some(TokenKind::Use) if visibility == Visibility::Private => {
                self.parse_use_declaration().map(Item::UseDeclaration)
            }
            Some(TokenKind::Use) => Err(self.error("`use` declarations cannot be public")),
            Some(TokenKind::Extern) => self
                .parse_extern_block(visibility, item_start)
                .map(Item::ExternBlock),
            Some(TokenKind::Type) => self
                .parse_type_declaration(visibility, item_start)
                .map(Item::TypeDeclaration),
            Some(TokenKind::Macro) => self
                .parse_macro_declaration(visibility, item_start)
                .map(Item::MacroDeclaration),
            _ => self
                .parse_statement_with_visibility(visibility, Some(item_start))
                .map(|statement| Item::Statement(Box::new(statement))),
        };
        self.newline_terminates_expression = previous;
        item
    }

    fn parse_macro_declaration(
        &mut self,
        visibility: Visibility,
        start: usize,
    ) -> Result<MacroDeclaration, ParseError> {
        self.expect(TokenKind::Macro, "expected `macro`")?;
        let name = self
            .expect(TokenKind::Identifier, "expected macro name")?
            .text;
        Ok(MacroDeclaration {
            syntax: self.syntax(start),
            visibility,
            name,
        })
    }

    /// Parses a private statement in a block expression.
    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        self.parse_statement_with_visibility(Visibility::Private, None)
    }

    /// Parses a statement, applying `visibility` to declarations.
    fn parse_statement_with_visibility(
        &mut self,
        visibility: Visibility,
        start: Option<usize>,
    ) -> Result<Statement, ParseError> {
        match self.peek() {
            Some(TokenKind::Let | TokenKind::Def) => self
                .parse_binding(visibility, start, start.is_some())
                .map(Statement::Binding),
            _ if visibility == Visibility::Public => {
                Err(self.error("`pub` must modify a declaration"))
            }
            _ => self.parse_expression().map(Statement::Expression),
        }
    }

    /// Parses a namespace, glob, selected, or renamed `use` declaration.
    fn parse_use_declaration(&mut self) -> Result<UseDeclaration, ParseError> {
        let start = self.position;
        self.expect(TokenKind::Use, "expected `use`")?;
        let mut path = vec![
            self.expect(TokenKind::Identifier, "expected module path after `use`")?
                .text,
        ];
        let kind = loop {
            if self.eat(TokenKind::As) {
                if path.len() < 2 {
                    return Err(self.error("renamed imports require a module path and item"));
                }
                let item = path.pop().expect("checked path length");
                let alias = self.parse_value_name("expected import alias after `as`")?;
                break UseKind::Renamed { item, alias };
            }
            if !self.eat(TokenKind::Dot) {
                break UseKind::Namespace;
            }
            if self.at(TokenKind::Star) && self.peek_n(1) != Some(TokenKind::As) {
                self.bump_token();
                break UseKind::Glob;
            }
            if self.eat(TokenKind::LParen) {
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
                break UseKind::Selected(names);
            }
            path.push(self.parse_value_name("expected module path component after `.`")?);
        };
        Ok(UseDeclaration {
            syntax: self.syntax(start),
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
            let binding = self.parse_binding(Visibility::Private, None, true)?;
            if !binding.type_parameters.is_empty() {
                return Err(self.error("external bindings cannot have compile-time parameters"));
            }
            bindings.push(binding);
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
        start: usize,
    ) -> Result<TypeDeclaration, ParseError> {
        self.expect(TokenKind::Type, "expected `type`")?;
        let kind = if self.eat(TokenKind::Alias) {
            TypeDeclarationKind::Alias
        } else {
            TypeDeclarationKind::Distinct
        };
        let name = self
            .expect(TokenKind::Identifier, "expected type name")?
            .text;
        let underlying = if self.eat(TokenKind::Equals) {
            Some(())
        } else if kind == TypeDeclarationKind::Alias {
            return Err(self.error("expected `=` after type alias name"));
        } else {
            None
        };
        let mut type_parameters = Vec::new();
        let underlying = if underlying.is_some() {
            type_parameters = self.parse_type_parameters()?;
            Some(self.parse_type()?)
        } else {
            None
        };
        let kind = if underlying.is_none() {
            TypeDeclarationKind::Opaque
        } else {
            kind
        };
        Ok(TypeDeclaration {
            syntax: self.syntax(start),
            visibility,
            kind,
            name,
            type_parameters,
            underlying,
        })
    }

    /// Parses a `let` or `def` binding with optional type and value.
    fn parse_binding(
        &mut self,
        visibility: Visibility,
        start: Option<usize>,
        allow_fixity: bool,
    ) -> Result<Binding, ParseError> {
        let start = start.unwrap_or(self.position);
        let kind = match self.peek() {
            Some(TokenKind::Let) => {
                self.bump_token();
                BindingKind::Let
            }
            Some(TokenKind::Def) => {
                self.bump_token();
                BindingKind::Def
            }
            _ => return Err(self.error("expected `let`, `def`, `type`, or `extern`")),
        };
        let fixity = match self.peek() {
            Some(TokenKind::Infix | TokenKind::Infixl | TokenKind::Infixr) => {
                if !allow_fixity {
                    return Err(self.error("fixity modifiers are only allowed at module level"));
                }
                let associativity = match self.bump_token().expect("peeked fixity").kind {
                    TokenKind::Infix => Associativity::None,
                    TokenKind::Infixl => Associativity::Left,
                    TokenKind::Infixr => Associativity::Right,
                    _ => unreachable!(),
                };
                let precedence = self
                    .expect(TokenKind::Integer, "expected precedence after fixity")?
                    .text
                    .parse::<u8>()
                    .map_err(|_| self.error("operator precedence must be between 0 and 9"))?;
                if precedence > 9 {
                    return Err(self.error("operator precedence must be between 0 and 9"));
                }
                Some(Fixity {
                    associativity,
                    precedence,
                })
            }
            _ => None,
        };
        let (name, consumed_colon) = self.parse_binding_name()?;
        let annotation = if consumed_colon || self.eat(TokenKind::Colon) {
            Some(())
        } else {
            None
        };
        let mut type_parameters = Vec::new();
        let annotation = if annotation.is_some() {
            type_parameters = self.parse_type_parameters()?;
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
            name,
            fixity,
            type_parameters,
            annotation,
            value,
        })
    }

    fn parse_type_parameters(&mut self) -> Result<Vec<TypeParameterPattern>, ParseError> {
        let mut parameters = Vec::new();
        loop {
            let checkpoint = self.position;
            let Ok(parameter) = self.parse_type_parameter_pattern() else {
                self.position = checkpoint;
                break;
            };
            if !self.eat(TokenKind::FatArrow) {
                self.position = checkpoint;
                break;
            }
            parameters.push(parameter);
        }
        Ok(parameters)
    }

    fn parse_type_parameter_pattern(&mut self) -> Result<TypeParameterPattern, ParseError> {
        let start = self.position;
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
        }))
    }

    /// Parses an expression, including function expressions and infix calls.
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
        let expression = self.parse_infix_expression()?;
        if matches!(self.peek(), Some(TokenKind::Arrow | TokenKind::FatArrow)) {
            return Err(function_error);
        }
        Ok(expression)
    }

    /// Parses a function parameter pattern, optional result type, and body.
    fn parse_function_expression(&mut self) -> Result<FunctionExpression, ParseError> {
        let start = self.position;
        let pattern = self.parse_pattern()?;
        let return_type = if self.eat(TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(TokenKind::FatArrow, "expected `=>` before function body")?;
        let body = Box::new(self.parse_expression()?);
        Ok(FunctionExpression {
            syntax: self.syntax(start),
            pattern,
            return_type,
            body,
        })
    }

    /// Parses either a binding pattern or a nested product pattern.
    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        let start = self.position;
        if self.eat(TokenKind::LParen) {
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
            Ok(Pattern::Product(ProductPattern {
                syntax: self.syntax(start),
                elements,
            }))
        } else {
            self.parse_binding_pattern().map(Pattern::Binding)
        }
    }

    /// Parses a named binding pattern whose type may be supplied contextually.
    fn parse_binding_pattern(&mut self) -> Result<BindingPattern, ParseError> {
        let start = self.position;
        let name = self
            .expect(TokenKind::Identifier, "expected parameter name")?
            .text;
        let ty = if self.eat(TokenKind::Colon) {
            self.parse_type_application()?
        } else {
            Type::Inferred(InferredType {
                syntax: self.syntax(start),
            })
        };
        Ok(BindingPattern {
            syntax: self.syntax(start),
            name,
            ty,
        })
    }

    /// Parses a type, treating function arrows as right-associative.
    fn parse_type(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        let parameter = self.parse_type_application()?;
        if self.eat(TokenKind::Arrow) {
            let result = self.parse_type()?;
            Ok(Type::Function(FunctionType {
                syntax: self.syntax(start),
                parameter: Box::new(parameter),
                result: Box::new(result),
            }))
        } else {
            Ok(parameter)
        }
    }

    fn parse_type_application(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        let mut ty = self.parse_type_atom()?;
        while self.starts_type_atom() {
            let argument = self.parse_type_atom()?;
            ty = Type::Application(TypeApplication {
                syntax: self.syntax(start),
                callee: Box::new(ty),
                argument: Box::new(argument),
            });
        }
        Ok(ty)
    }

    fn starts_type_atom(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                TokenKind::Underscore | TokenKind::Star | TokenKind::LParen | TokenKind::Identifier
            )
        )
    }

    /// Parses a non-function type such as a primitive, pointer, product, or name.
    fn parse_type_atom(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        if self.eat(TokenKind::Underscore) {
            return Ok(Type::Inferred(InferredType {
                syntax: self.syntax(start),
            }));
        }
        if self.eat(TokenKind::Star) {
            let is_const = self.eat(TokenKind::Const);
            let pointee = self.parse_type_application()?;
            return Ok(Type::Pointer(PointerType {
                syntax: self.syntax(start),
                is_const,
                pointee: Box::new(pointee),
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
                        variadic = true;
                    } else {
                        let ty = self.parse_type()?;
                        elements.push(TypeElement {
                            syntax: self.syntax(element_start),
                            name,
                            ty,
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
        let (namespace, name) = if self.eat(TokenKind::Dot) {
            let name = self
                .expect(TokenKind::Identifier, "expected type name after namespace")?
                .text;
            (Some(first), name)
        } else {
            (None, first)
        };
        Ok(Type::Named(NamedType {
            syntax: self.syntax(start),
            namespace,
            name,
        }))
    }

    /// Parses a flat infix chain for fixity-aware lowering during resolution.
    fn parse_infix_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut operands = vec![self.parse_call_expression()?];
        let mut operators = Vec::new();
        while let Some(operator) = self.parse_infix_operator()? {
            operators.push(operator);
            operands.push(self.parse_call_expression()?);
        }
        if operators.is_empty() {
            Ok(operands.pop().expect("one operand"))
        } else {
            Ok(Expression::Infix(InfixExpression {
                syntax: self.syntax(start),
                operands,
                operators,
            }))
        }
    }

    fn parse_infix_operator(&mut self) -> Result<Option<InfixOperator>, ParseError> {
        if self.newline_terminates_expression && self.has_newline_before_next_token() {
            return Ok(None);
        }
        let start = self.position;
        if self.eat(TokenKind::Backtick) {
            let first = self
                .expect(
                    TokenKind::Identifier,
                    "expected function name after backtick",
                )?
                .text;
            let (namespace, name) = if self.eat(TokenKind::Dot) {
                let name = self
                    .expect(TokenKind::Identifier, "expected qualified function name")?
                    .text;
                (Some(first), name)
            } else {
                (None, first)
            };
            self.expect(TokenKind::Backtick, "expected closing backtick")?;
            return Ok(Some(InfixOperator {
                syntax: self.syntax(start),
                namespace,
                name,
            }));
        }
        if self.peek() == Some(TokenKind::Identifier)
            && self.peek_n(1) == Some(TokenKind::Dot)
            && self.peek_n(2).is_some_and(is_symbol_kind)
        {
            let namespace = self.bump_token().expect("peeked namespace").text;
            self.expect(TokenKind::Dot, "expected `.`")?;
            let name = self.bump_token().expect("peeked operator").text;
            return Ok(Some(InfixOperator {
                syntax: self.syntax(start),
                namespace: Some(namespace),
                name,
            }));
        }
        if self.peek().is_some_and(is_symbol_kind) {
            let name = self.bump_token().expect("peeked operator").text;
            return Ok(Some(InfixOperator {
                syntax: self.syntax(start),
                namespace: None,
                name,
            }));
        }
        Ok(None)
    }

    /// Parses juxtaposition-based function calls.
    fn parse_call_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_access_expression()?;
        while self.starts_atom() {
            let argument = self.parse_access_expression()?;
            expression = Expression::Call(CallExpression {
                syntax: self.syntax(start),
                callee: Box::new(expression),
                argument: Box::new(argument),
            });
        }
        Ok(expression)
    }

    /// Parses chained named or positional product access.
    fn parse_access_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        let mut expression = self.parse_atom()?;
        while self.eat(TokenKind::Dot) {
            let accessor = match self.peek() {
                Some(TokenKind::Identifier) => {
                    Accessor::Name(self.bump_token().expect("peeked name").text)
                }
                Some(TokenKind::Integer) => {
                    Accessor::Index(self.bump_token().expect("peeked index").text)
                }
                _ => return Err(self.error("expected a product element name or index after `.`")),
            };
            expression = Expression::Access(AccessExpression {
                syntax: self.syntax(start),
                value: Box::new(expression),
                accessor,
            });
        }
        Ok(expression)
    }

    /// Parses a primary expression without call, access, or binary operators.
    fn parse_atom(&mut self) -> Result<Expression, ParseError> {
        match self.peek() {
            Some(TokenKind::LBrace) => self.parse_block_expression().map(Expression::Block),
            Some(TokenKind::LParen) if self.parenthesized_operator() => self.parse_operator_value(),
            Some(TokenKind::LParen) => self.parse_product_expression().map(Expression::Product),
            Some(TokenKind::Identifier) => {
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
            _ => Err(self.error("expected expression")),
        }
    }

    /// Parses a brace-delimited sequence of statements.
    fn parse_block_expression(&mut self) -> Result<BlockExpression, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LBrace, "expected `{`")?;
        let mut statements = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated block expression"));
            }
            statements.push(self.parse_statement()?);
        }
        self.expect(TokenKind::RBrace, "expected `}`")?;
        Ok(BlockExpression {
            syntax: self.syntax(start),
            statements,
        })
    }

    /// Parses a parenthesized product expression with optional element names.
    fn parse_product_expression(&mut self) -> Result<ProductExpression, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LParen, "expected `(`")?;
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
                let value = self.parse_expression()?;
                elements.push(ProductElement {
                    syntax: self.syntax(element_start),
                    name,
                    value,
                });
                if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen, "expected `)` after product")?;
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
        if self.peek() == Some(TokenKind::Identifier)
            && self.peek_n(1) == Some(TokenKind::Dot)
            && self.peek_n(2).is_some_and(is_symbol_kind)
        {
            return false;
        }
        matches!(
            self.peek(),
            Some(
                TokenKind::LBrace
                    | TokenKind::LParen
                    | TokenKind::Identifier
                    | TokenKind::String
                    | TokenKind::Integer
            )
        )
    }

    fn parenthesized_operator(&self) -> bool {
        self.peek() == Some(TokenKind::LParen)
            && (self.peek_n(1).is_some_and(is_symbol_kind)
                && self.peek_n(2) == Some(TokenKind::RParen)
                || self.peek_n(1) == Some(TokenKind::Identifier)
                    && self.peek_n(2) == Some(TokenKind::Dot)
                    && self.peek_n(3).is_some_and(is_symbol_kind)
                    && self.peek_n(4) == Some(TokenKind::RParen))
    }

    fn parse_operator_value(&mut self) -> Result<Expression, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LParen, "expected `(`")?;
        let first = self.bump_token().expect("peeked operator value");
        let qualified_operator = if first.kind == TokenKind::Identifier {
            self.expect(TokenKind::Dot, "expected `.`")?;
            let operator = self.bump_token().expect("peeked qualified operator");
            Some(operator.text)
        } else {
            None
        };
        self.expect(TokenKind::RParen, "expected `)` after operator")?;
        let expression = if let Some(operator) = qualified_operator {
            let namespace = Expression::Name(NameExpression {
                syntax: self.syntax(start),
                name: first.text,
            });
            Expression::Access(AccessExpression {
                syntax: self.syntax(start),
                value: Box::new(namespace),
                accessor: Accessor::Name(operator),
            })
        } else {
            Expression::Name(NameExpression {
                syntax: self.syntax(start),
                name: first.text,
            })
        };
        Ok(expression)
    }

    fn parse_value_name(&mut self, message: &'static str) -> Result<String, ParseError> {
        if self.peek() == Some(TokenKind::Identifier) || self.peek().is_some_and(is_symbol_kind) {
            Ok(self.bump_token().expect("peeked value name").text)
        } else {
            Err(self.error(message))
        }
    }

    fn parse_binding_name(&mut self) -> Result<(String, bool), ParseError> {
        let mut name = self.parse_value_name("expected binding name")?;
        let consumed_colon =
            name.len() > 1 && name.ends_with(':') && self.peek().is_some_and(is_type_atom_start);
        if consumed_colon {
            name.pop();
        }
        Ok((name, consumed_colon))
    }

    fn parse_import_name(&mut self) -> Result<String, ParseError> {
        if self.eat(TokenKind::LParen) {
            let name = self.parse_value_name("expected operator in imported name")?;
            self.expect(TokenKind::RParen, "expected `)` after imported operator")?;
            Ok(name)
        } else {
            self.parse_value_name("expected imported item name")
        }
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

fn is_symbol_kind(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Operator
            | TokenKind::Colon
            | TokenKind::Dot
            | TokenKind::Equals
            | TokenKind::Star
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Slash
    )
}

fn is_type_atom_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Underscore | TokenKind::Star | TokenKind::LParen | TokenKind::Identifier
    )
}
