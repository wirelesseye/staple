use crate::ast::*;
use crate::lexer::lex;
use std::sync::Arc;

/// Parse a complete staple source file.
pub fn parse(source: &str) -> Result<Module, ParseError> {
    let tokens = Arc::from(lex(source));
    Grammar::new(tokens, source.len(), 0, None).parse_source_file()
}

pub(crate) fn parse_with_syntax_ids(
    source: &str,
    next_syntax_id: &mut usize,
    source_name: &str,
) -> Result<Module, ParseError> {
    let tokens = Arc::from(lex(source));
    let module = Grammar::new(
        tokens,
        source.len(),
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
    source_len: usize,
    next_syntax_id: usize,
    newline_terminates_expression: bool,
    source_name: Option<Arc<str>>,
}

impl Grammar {
    fn new(
        tokens: Arc<[SyntaxToken]>,
        source_len: usize,
        next_syntax_id: usize,
        source_name: Option<Arc<str>>,
    ) -> Self {
        Self {
            tokens,
            position: 0,
            source_len,
            next_syntax_id,
            newline_terminates_expression: false,
            source_name,
        }
    }

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
            _ => self
                .parse_statement_with_visibility(visibility, Some(item_start))
                .map(|statement| Item::Statement(Box::new(statement))),
        };
        self.newline_terminates_expression = previous;
        item
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        self.parse_statement_with_visibility(Visibility::Private, None)
    }

    fn parse_statement_with_visibility(
        &mut self,
        visibility: Visibility,
        start: Option<usize>,
    ) -> Result<Statement, ParseError> {
        match self.peek() {
            Some(TokenKind::Let | TokenKind::Def) => self
                .parse_binding(visibility, start)
                .map(Statement::Binding),
            _ if visibility == Visibility::Public => {
                Err(self.error("`pub` must modify a declaration"))
            }
            _ => self.parse_expression().map(Statement::Expression),
        }
    }

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
                let alias = self
                    .expect(TokenKind::Identifier, "expected import alias after `as`")?
                    .text;
                break UseKind::Renamed { item, alias };
            }
            if !self.eat(TokenKind::Dot) {
                break UseKind::Namespace;
            }
            if self.eat(TokenKind::Star) {
                break UseKind::Glob;
            }
            if self.eat(TokenKind::LParen) {
                let mut names = Vec::new();
                if !self.at(TokenKind::RParen) {
                    loop {
                        names.push(
                            self.expect(TokenKind::Identifier, "expected imported item name")?
                                .text,
                        );
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
            path.push(
                self.expect(
                    TokenKind::Identifier,
                    "expected module path component after `.`",
                )?
                .text,
            );
        };
        Ok(UseDeclaration {
            syntax: self.syntax(start),
            path,
            kind,
        })
    }

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
            bindings.push(self.parse_binding(Visibility::Private, None)?);
        }
        self.expect(TokenKind::RBrace, "expected `}`")?;
        Ok(ExternBlock {
            syntax: self.syntax(start),
            visibility,
            abi,
            bindings,
        })
    }

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
        self.expect(TokenKind::Equals, "expected `=` after type name")?;
        let underlying = self.parse_type()?;
        Ok(TypeDeclaration {
            syntax: self.syntax(start),
            visibility,
            kind,
            name,
            underlying,
        })
    }

    fn parse_binding(
        &mut self,
        visibility: Visibility,
        start: Option<usize>,
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
        let name = self
            .expect(TokenKind::Identifier, "expected binding name")?
            .text;
        let annotation = if self.eat(TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
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
            annotation,
            value,
        })
    }

    fn parse_expression(&mut self) -> Result<Expression, ParseError> {
        let checkpoint = self.position;
        let function_error = match self.parse_function_expression() {
            Ok(function) => return Ok(Expression::Function(Box::new(function))),
            Err(error) => error,
        };
        self.position = checkpoint;
        let expression = self.parse_binary_expression(0)?;
        if matches!(self.peek(), Some(TokenKind::Arrow | TokenKind::FatArrow)) {
            return Err(function_error);
        }
        Ok(expression)
    }

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

    fn parse_binding_pattern(&mut self) -> Result<BindingPattern, ParseError> {
        let start = self.position;
        let name = self
            .expect(TokenKind::Identifier, "expected parameter name")?
            .text;
        self.expect(TokenKind::Colon, "expected `:` after parameter name")?;
        let ty = self.parse_type_atom()?;
        Ok(BindingPattern {
            syntax: self.syntax(start),
            name,
            ty,
        })
    }

    fn parse_type(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        let parameter = self.parse_type_atom()?;
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

    fn parse_type_atom(&mut self) -> Result<Type, ParseError> {
        let start = self.position;
        if self.eat(TokenKind::I32) {
            return Ok(Type::Primitive(PrimitiveType::I32(self.syntax(start))));
        }
        if self.eat(TokenKind::Bool) {
            return Ok(Type::Primitive(PrimitiveType::Bool(self.syntax(start))));
        }
        if self.eat(TokenKind::Underscore) {
            return Ok(Type::Inferred(InferredType {
                syntax: self.syntax(start),
            }));
        }
        if self.eat(TokenKind::Star) {
            let is_const = self.eat(TokenKind::Const);
            let pointee = self.parse_type_atom()?;
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

    fn parse_binary_expression(
        &mut self,
        minimum_precedence: u8,
    ) -> Result<Expression, ParseError> {
        let mut left = self.parse_call_expression()?;
        while let Some((precedence, token_kind, operator)) = self.binary_operator() {
            if precedence < minimum_precedence {
                break;
            }
            let start = self.token_index_at(left.syntax().span.to_range().start);
            self.expect(token_kind, "expected binary operator")?;
            let right = self.parse_binary_expression(precedence + 1)?;
            left = Expression::Binary(BinaryExpression {
                syntax: self.syntax(start),
                operator,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Ok(left)
    }

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

    fn parse_atom(&mut self) -> Result<Expression, ParseError> {
        match self.peek() {
            Some(TokenKind::LBrace) => self.parse_block_expression().map(Expression::Block),
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

    fn starts_atom(&self) -> bool {
        if self.newline_terminates_expression && self.has_newline_before_next_token() {
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

    fn binary_operator(&self) -> Option<(u8, TokenKind, BinaryOperator)> {
        if self.newline_terminates_expression && self.has_newline_before_next_token() {
            return None;
        }
        match self.peek()? {
            TokenKind::Plus => Some((1, TokenKind::Plus, BinaryOperator::Add)),
            TokenKind::Minus => Some((1, TokenKind::Minus, BinaryOperator::Subtract)),
            TokenKind::Star => Some((2, TokenKind::Star, BinaryOperator::Multiply)),
            TokenKind::Slash => Some((2, TokenKind::Slash, BinaryOperator::Divide)),
            _ => None,
        }
    }

    fn peek(&self) -> Option<TokenKind> {
        self.peek_n(0)
    }

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

    fn at(&self, kind: TokenKind) -> bool {
        self.peek() == Some(kind)
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.bump_token();
            true
        } else {
            false
        }
    }

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

    fn bump_token(&mut self) -> Option<SyntaxToken> {
        self.position = self.next_non_trivia(self.position);
        let token = self.tokens.get(self.position)?.clone();
        self.position += 1;
        Some(token)
    }

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

    fn has_newline_before_next_token(&self) -> bool {
        self.tokens[self.position..self.next_non_trivia(self.position)]
            .iter()
            .any(|token| token.kind == TokenKind::Newline)
    }

    fn syntax(&mut self, start: usize) -> Syntax {
        let tokens = &self.tokens[start..self.position];
        let span_start = tokens
            .first()
            .map_or(self.source_len, |token| token.span.start);
        let span_end = tokens.last().map_or(span_start, |token| token.span.end);
        let id = SyntaxId(self.next_syntax_id);
        self.next_syntax_id += 1;
        Syntax {
            id,
            span: Span::User {
                source: self.source_name.clone(),
                range: span_start..span_end,
            },
            tokens: Arc::clone(&self.tokens),
            token_range: start..self.position,
        }
    }

    fn token_index_at(&self, byte_offset: usize) -> usize {
        self.tokens
            .partition_point(|token| token.span.end <= byte_offset)
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        let position = self.next_non_trivia(self.position);
        ParseError {
            offset: self
                .tokens
                .get(position)
                .map_or(self.source_len, |token| token.span.start),
            message: message.into(),
        }
    }
}
