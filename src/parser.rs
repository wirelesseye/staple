use crate::ast::{NodeKind, ParseError, SyntaxNode, SyntaxToken, TokenKind};
use crate::lexer::lex;

/// Parse a complete staple source file.
pub fn parse(source: &str) -> Result<SyntaxNode, ParseError> {
    let tokens = lex(source);
    Grammar::new(tokens, source.len()).parse_source_file()
}

struct Grammar {
    tokens: Vec<SyntaxToken>,
    position: usize,
    source_len: usize,
}

impl Grammar {
    fn new(tokens: Vec<SyntaxToken>, source_len: usize) -> Self {
        Self {
            tokens,
            position: 0,
            source_len,
        }
    }

    fn parse_source_file(mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        let mut children = Vec::new();
        while self.peek().is_some() {
            children.push(self.parse_declaration()?);
        }
        // The final declaration does not own trailing trivia. Keep it on the
        // source-file node so a complete parse always round-trips byte-for-byte.
        self.position = self.tokens.len();
        Ok(self.node(NodeKind::SourceFile, start, children))
    }

    fn parse_declaration(&mut self) -> Result<SyntaxNode, ParseError> {
        match self.peek() {
            Some(TokenKind::Extern) => self.parse_extern_block(),
            Some(TokenKind::Type) => self.parse_type_declaration(),
            _ => self.parse_binding(),
        }
    }

    fn parse_extern_block(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        self.expect(TokenKind::Extern, "expected `extern`")?;
        self.expect(TokenKind::String, "expected ABI string after `extern`")?;
        self.expect(TokenKind::LBrace, "expected `{` after extern ABI")?;
        let mut children = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated extern block"));
            }
            children.push(self.parse_binding()?);
        }
        self.expect(TokenKind::RBrace, "expected `}`")?;
        Ok(self.node(NodeKind::ExternBlock, start, children))
    }

    fn parse_type_declaration(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        self.expect(TokenKind::Type, "expected `type`")?;
        let kind = if self.eat(TokenKind::Alias) {
            NodeKind::TypeAlias
        } else {
            NodeKind::TypeDefinition
        };
        self.expect(TokenKind::Identifier, "expected type name")?;
        self.expect(TokenKind::Equals, "expected `=` after type name")?;
        let underlying_type = self.parse_type()?;
        Ok(self.node(kind, start, vec![underlying_type]))
    }

    fn parse_binding(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        let binding_kind = match self.peek() {
            Some(TokenKind::Let) => NodeKind::LetBinding,
            Some(TokenKind::Def) => NodeKind::DefBinding,
            _ => return Err(self.error("expected `let`, `def`, or `extern`")),
        };
        self.bump();
        self.expect(TokenKind::Identifier, "expected binding name")?;
        let mut children = Vec::new();
        if self.eat(TokenKind::Colon) {
            children.push(self.parse_type()?);
        }
        if self.eat(TokenKind::Equals) {
            children.push(self.parse_expression()?);
        }
        Ok(self.node(binding_kind, start, children))
    }

    fn parse_expression(&mut self) -> Result<SyntaxNode, ParseError> {
        let checkpoint = self.position;
        if let Ok(function) = self.parse_function_value() {
            return Ok(function);
        }
        self.position = checkpoint;
        self.parse_binary_expression(0)
    }

    fn parse_function_value(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        let parameter = self.parse_parameter()?;
        self.expect(TokenKind::Arrow, "expected `->` after function parameter")?;
        let body = self.parse_expression()?;
        Ok(self.node(NodeKind::FunctionValue, start, vec![parameter, body]))
    }

    fn parse_parameter(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        let mut children = Vec::new();
        if self.eat(TokenKind::LParen) {
            if !self.at(TokenKind::RParen) {
                loop {
                    self.expect(TokenKind::Identifier, "expected parameter name")?;
                    self.expect(TokenKind::Colon, "expected `:` after parameter name")?;
                    children.push(self.parse_atomic_type()?);
                    if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RParen, "expected `)` after parameter")?;
        } else {
            self.expect(TokenKind::Identifier, "expected function parameter")?;
            self.expect(TokenKind::Colon, "expected `:` after parameter name")?;
            children.push(self.parse_atomic_type()?);
        }
        Ok(self.node(NodeKind::Parameter, start, children))
    }

    fn parse_atomic_type(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        self.parse_type_atom()?;
        Ok(self.node(NodeKind::Type, start, Vec::new()))
    }

    fn parse_type(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        self.parse_type_atom()?;
        if self.eat(TokenKind::Arrow) {
            self.parse_type()?;
        }
        Ok(self.node(NodeKind::Type, start, Vec::new()))
    }

    fn parse_type_atom(&mut self) -> Result<(), ParseError> {
        if self.eat(TokenKind::Underscore) {
            return Ok(());
        }
        if self.eat(TokenKind::Star) {
            self.eat(TokenKind::Const);
            self.expect(TokenKind::Identifier, "expected pointee type")?;
            return Ok(());
        }
        if self.eat(TokenKind::LParen) {
            if !self.at(TokenKind::RParen) {
                loop {
                    if self.eat(TokenKind::Ellipsis) {
                        // A variadic marker must be the final list item.
                    } else {
                        if self.peek() == Some(TokenKind::Identifier)
                            && self.peek_n(1) == Some(TokenKind::Colon)
                        {
                            self.bump();
                            self.expect(TokenKind::Colon, "expected `:` after element name")?;
                        }
                        self.parse_type()?;
                    }
                    if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RParen, "expected `)` in type")?;
            return Ok(());
        }
        self.expect(TokenKind::Identifier, "expected type")?;
        Ok(())
    }

    fn parse_binary_expression(
        &mut self,
        minimum_precedence: u8,
    ) -> Result<SyntaxNode, ParseError> {
        let mut left = self.parse_call_expression()?;
        while let Some((precedence, operator)) = self.binary_operator() {
            if precedence < minimum_precedence {
                break;
            }
            let start = left
                .tokens
                .first()
                .map_or(self.position, |_| self.token_index_at(left.span.start));
            self.expect(operator, "expected binary operator")?;
            let right = self.parse_binary_expression(precedence + 1)?;
            left = self.node(NodeKind::BinaryExpression, start, vec![left, right]);
        }
        Ok(left)
    }

    fn parse_call_expression(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        let callee = self.parse_access_expression()?;
        let mut children = vec![callee];
        while self.starts_atom() {
            children.push(self.parse_access_expression()?);
        }
        if children.len() == 1 {
            Ok(children.pop().expect("one child"))
        } else {
            Ok(self.node(NodeKind::CallExpression, start, children))
        }
    }

    fn parse_access_expression(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        let mut value = self.parse_atom()?;
        while self.eat(TokenKind::Dot) {
            let accessor = match self.peek() {
                Some(TokenKind::Identifier) => self.leaf(NodeKind::NameExpression)?,
                Some(TokenKind::Integer) => self.leaf(NodeKind::IntegerExpression)?,
                _ => return Err(self.error("expected a list element name or index after `.`")),
            };
            value = self.node(NodeKind::AccessExpression, start, vec![value, accessor]);
        }
        Ok(value)
    }

    fn parse_atom(&mut self) -> Result<SyntaxNode, ParseError> {
        match self.peek() {
            Some(TokenKind::LBrace) => self.parse_block_expression(),
            Some(TokenKind::LParen) => self.parse_list_expression(),
            Some(TokenKind::Identifier) => self.leaf(NodeKind::NameExpression),
            Some(TokenKind::String) => self.leaf(NodeKind::StringExpression),
            Some(TokenKind::Integer) => self.leaf(NodeKind::IntegerExpression),
            _ => Err(self.error("expected expression")),
        }
    }

    fn parse_block_expression(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LBrace, "expected `{`")?;
        let mut children = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.peek().is_none() {
                return Err(self.error("unterminated block expression"));
            }
            children.push(match self.peek() {
                Some(TokenKind::Let | TokenKind::Def) => self.parse_binding()?,
                _ => self.parse_expression()?,
            });
        }
        self.expect(TokenKind::RBrace, "expected `}`")?;
        Ok(self.node(NodeKind::BlockExpression, start, children))
    }

    fn parse_list_expression(&mut self) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        self.expect(TokenKind::LParen, "expected `(`")?;
        let mut children = Vec::new();
        if !self.at(TokenKind::RParen) {
            loop {
                let element_start = self.position;
                if self.peek() == Some(TokenKind::Identifier)
                    && self.peek_n(1) == Some(TokenKind::Colon)
                {
                    self.bump();
                    self.expect(TokenKind::Colon, "expected `:` after element name")?;
                    let value = self.parse_expression()?;
                    children.push(self.node(
                        NodeKind::NamedListElement,
                        element_start,
                        vec![value],
                    ));
                } else {
                    children.push(self.parse_expression()?);
                }
                if !self.eat(TokenKind::Comma) || self.at(TokenKind::RParen) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen, "expected `)` after list")?;
        Ok(self.node(NodeKind::ListExpression, start, children))
    }

    fn leaf(&mut self, kind: NodeKind) -> Result<SyntaxNode, ParseError> {
        let start = self.position;
        self.bump();
        Ok(self.node(kind, start, Vec::new()))
    }

    fn starts_atom(&self) -> bool {
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

    fn binary_operator(&self) -> Option<(u8, TokenKind)> {
        match self.peek()? {
            TokenKind::Plus | TokenKind::Minus => Some((1, self.peek()?)),
            TokenKind::Star | TokenKind::Slash => Some((2, self.peek()?)),
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
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind, message: &'static str) -> Result<(), ParseError> {
        self.eat(kind)
            .then_some(())
            .ok_or_else(|| self.error(message))
    }

    fn bump(&mut self) {
        self.position = self.next_non_trivia(self.position);
        if self.position < self.tokens.len() {
            self.position += 1;
        }
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

    fn node(&self, kind: NodeKind, start: usize, children: Vec<SyntaxNode>) -> SyntaxNode {
        let end = self.position;
        let tokens = self.tokens[start..end].to_vec();
        let span_start = tokens
            .first()
            .map_or(self.source_len, |token| token.span.start);
        let span_end = tokens.last().map_or(span_start, |token| token.span.end);
        SyntaxNode {
            kind,
            span: span_start..span_end,
            tokens,
            children,
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
