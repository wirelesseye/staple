//! Syntax operations shared by the macro interpreter and the syntax runtime.
//!
//! Everything here works on `Syntax` and AST nodes only. The interpreter calls
//! these helpers while parsing and filling quotations, and compiled macro code
//! reaches the same behaviour through the host intrinsics.

use std::collections::HashMap;
use std::sync::Arc;

use staple_syntax::{
    Expression, Item, ModifierInvocation, NameExpression, Pattern, Span, Syntax, SyntaxId,
    SyntaxToken, TokenKind, Type, TypeBody, TypeParameter, TypeParameters, VisibilityKind,
    VisibilitySyntax,
};

/// The syntax a slice of `parent`'s token stream covers.
pub(crate) fn syntax_slice(parent: &Syntax, start: usize, end: usize) -> Syntax {
    let mut syntax = parent.clone();
    syntax.token_range = parent.token_range.start + start..parent.token_range.start + end;
    syntax
}

/// Where the first unread token of a stream is, for diagnostics.
pub(crate) fn describe_position(syntax: &Syntax, cursor: usize) -> String {
    let tokens = syntax.tokens();
    let Some(offset) = (cursor..tokens.len()).find(|index| !tokens[*index].kind.is_trivia()) else {
        return "end of input".to_owned();
    };
    let position = syntax.token_range.start + offset;
    let (mut line, mut column) = (1usize, 1usize);
    for token in &syntax.token_stream()[..position] {
        for character in token.text.chars() {
            if character == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
    }
    format!("line {line}, column {column}")
}

/// The first non-trivia token at or after `cursor`, up to `end`.
pub(crate) fn skip_trivia(tokens: &[SyntaxToken], mut cursor: usize, end: usize) -> usize {
    while cursor < end && tokens[cursor].kind.is_trivia() {
        cursor += 1;
    }
    cursor
}

/// Finds the token that closes the delimiter opened at `open`.
pub(crate) fn matching_close(tokens: &[SyntaxToken], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Finds the `>` that closes the angle bracket opened at `open`.
pub(crate) fn matching_angle_close(tokens: &[SyntaxToken], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut cursor = open;
    while let Some(token) = tokens.get(cursor) {
        match token.kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                cursor = matching_close(tokens, cursor)?;
            }
            TokenKind::Operator if token.text == "<" => depth += 1,
            TokenKind::Operator if token.text == ">" => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

/// The hygiene to give a spliced value's tokens that carry none of their own.
/// Tokens that went through a quotation already record where each was written,
/// so a raw fragment, or a value with any such token, keeps them.
pub(crate) fn root_hygiene(syntax: &Syntax) -> Option<(usize, u64)> {
    if syntax.tokens().iter().any(|token| token.hygiene.is_some()) {
        return None;
    }
    syntax.definition_module().zip(syntax.expansion_mark)
}

/// Appends `syntax`'s tokens to `output`, filling in the hygiene and origin of
/// each token that has none.
pub(crate) fn append_syntax_tokens(
    output: &mut Vec<SyntaxToken>,
    syntax: &Syntax,
    hygiene: Option<(usize, u64)>,
) {
    let mut occurrences = HashMap::<String, usize>::new();
    for token in syntax.tokens() {
        let mut token = token.clone();
        if token.hygiene.is_none() && !token.kind.is_trivia() {
            token.hygiene = hygiene;
        }
        if token.origin.is_none() && !token.kind.is_trivia() {
            let occurrence = occurrences.entry(token.text.clone()).or_default();
            token.origin = syntax
                .identifier_origins
                .iter()
                .filter(|(name, _)| name == &token.text)
                .nth(*occurrence)
                .map(|(_, origin)| origin.clone())
                .or_else(|| match &syntax.span {
                    Span::User { source, .. } => Some(Span::User {
                        source: source.clone(),
                        range: token.span.clone(),
                        location: None,
                    }),
                    Span::Compiler => None,
                });
            *occurrence += 1;
        }
        output.push(token);
    }
}

/// Whether `ty` is exposed as a structured `ProductType`: a plain comma
/// product without spreads, a variadic marker, or `mut`/`move` elements. Every
/// other type is an `UnstructuredType`.
pub(crate) fn is_structured_product_type(ty: &Type) -> bool {
    matches!(ty, Type::Product(product)
        if !product.slotted
            && !product.variadic
            && product
                .elements
                .iter()
                .all(|element| !element.spread && !element.mutable && !element.moved))
}

/// Whether `syntax` is exactly one token of `expected` kind.
pub(crate) fn matches_single_token(syntax: &Syntax, expected: TokenKind) -> bool {
    let mut tokens = syntax
        .tokens()
        .iter()
        .filter(|token| !token.kind.is_trivia());
    matches!(tokens.next(), Some(token) if token.kind == expected) && tokens.next().is_none()
}

/// The syntax an item covers.
pub(crate) fn item_syntax(item: &Item) -> &Syntax {
    match item {
        Item::Modified(value) => &value.syntax,
        Item::VisibilityMacroInvocation(value) => &value.syntax,
        Item::VisibilitySplice(value) => &value.syntax,
        Item::ItemSplice(value) => &value.syntax,
        Item::UseDeclaration(value) => &value.syntax,
        Item::Submodule(value) => &value.syntax,
        Item::ExternBlock(value) => &value.syntax,
        Item::TypeDeclaration(value) => &value.syntax,
        Item::MacroDeclaration(value) => &value.syntax,
        Item::TraitDeclaration(value) => &value.syntax,
        Item::TraitImplementation(value) => &value.syntax,
        value @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => block_item_syntax(value),
    }
}

fn block_item_syntax(item: &Item) -> &Syntax {
    match item {
        Item::Binding(value) => &value.syntax,
        Item::PatternBinding(value) => &value.syntax,
        Item::Assignment(value) => &value.syntax,
        Item::Return(value) => &value.syntax,
        Item::Break(value) => &value.syntax,
        Item::Continue(value) => &value.syntax,
        Item::Expression(value) => value.syntax(),
        Item::Submodule(value) => &value.syntax,
        Item::TypeDeclaration(value) => &value.syntax,
        Item::UseDeclaration(value) => &value.syntax,
        _ => unreachable!("item-only syntax helper received a declaration item"),
    }
}

/// Lexes `text` into one lossless syntax fragment. The caller owns the next
/// syntax id so the result fits the program's id space.
pub(crate) fn syntax_from_text(text: &str, next_syntax_id: &mut usize) -> Syntax {
    let tokens: Arc<[SyntaxToken]> = staple_syntax::lex(text).into();
    let id = SyntaxId(*next_syntax_id);
    *next_syntax_id += 1;
    Syntax {
        pipe_stage: false,
        implied: false,
        id,
        span: Span::User {
            source: None,
            range: 0..text.len(),
            location: None,
        },
        token_range: 0..tokens.len(),
        tokens,
        definition_module: None,
        expansion_mark: None,
        identifier_origins: Vec::new(),
    }
}

/// A syntax leaf a `Parse` implementation reads in one step. The interpreter
/// and the syntax runtime both use [`parse_leaf`], so their accepted grammar
/// and diagnostics stay the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeafKind {
    Expr,
    CallExpr,
    CharExpr,
    StringExpr,
    UnstructuredExpr,
    Type,
    ProductType,
    UnstructuredType,
    Pattern,
    BindingPattern,
    NominalPattern,
    Item,
    ModifiedItem,
    TypeDeclarationItem,
    UnstructuredItem,
    Modifier,
    ModifierCall,
    UnstructuredModifier,
    TypeBody,
    TypeParameter,
    TypeParameters,
    Ident,
    Comma,
    Equals,
    FatArrow,
    Visibility,
}

impl LeafKind {
    /// The name a diagnostic uses for a failed parse.
    pub(crate) fn display(self, spelling: Option<&str>) -> String {
        match self {
            Self::Ident => match spelling {
                Some(spelling) => format!("Ident<{spelling:?}>"),
                None => "Ident<String>".to_owned(),
            },
            other => other.name().to_owned(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Expr => "Expr",
            Self::CallExpr => "CallExpr",
            Self::CharExpr => "CharExpr",
            Self::StringExpr => "StringExpr",
            Self::UnstructuredExpr => "UnstructuredExpr",
            Self::Type => "Type",
            Self::ProductType => "ProductType",
            Self::UnstructuredType => "UnstructuredType",
            Self::Pattern => "Pattern",
            Self::BindingPattern => "BindingPattern",
            Self::NominalPattern => "NominalPattern",
            Self::Item => "Item",
            Self::ModifiedItem => "ModifiedItem",
            Self::TypeDeclarationItem => "TypeDeclarationItem",
            Self::UnstructuredItem => "UnstructuredItem",
            Self::Modifier => "Modifier",
            Self::ModifierCall => "ModifierCall",
            Self::UnstructuredModifier => "UnstructuredModifier",
            Self::TypeBody => "TypeBody",
            Self::TypeParameter => "TypeParameter",
            Self::TypeParameters => "TypeParameters",
            Self::Ident => "Ident",
            Self::Comma => "Comma",
            Self::Equals => "Equals",
            Self::FatArrow => "FatArrow",
            Self::Visibility => "Visibility",
        }
    }
}

/// One parsed leaf and the tokens it used.
pub(crate) enum ParsedLeaf {
    Expression(Expression),
    Type(Type),
    Pattern(Pattern),
    Item(Item),
    Modifier(ModifierInvocation),
    TypeBody(TypeBody),
    TypeParameter(TypeParameter),
    TypeParameters(TypeParameters),
    Ident(NameExpression),
    Punctuation(LeafKind, Syntax),
    Visibility(VisibilitySyntax),
}

impl ParsedLeaf {
    pub(crate) fn into_syntax(self) -> Syntax {
        match self {
            Self::Expression(expression) => expression.syntax().clone(),
            Self::Type(ty) => ty.syntax().clone(),
            Self::Pattern(pattern) => pattern.syntax().clone(),
            Self::Item(item) => item_syntax(&item).clone(),
            Self::Modifier(modifier) => modifier.syntax,
            Self::TypeBody(body) => body.syntax,
            Self::TypeParameter(parameter) => parameter.syntax,
            Self::TypeParameters(parameters) => parameters.syntax,
            Self::Ident(ident) => ident.syntax,
            Self::Punctuation(_, syntax) => syntax,
            Self::Visibility(visibility) => visibility.syntax,
        }
    }
}

/// Why a leaf parse failed: the node had a different category, or the parser
/// rejected it with a message.
#[derive(Debug)]
pub(crate) enum LeafFailure {
    Mismatch,
    Parse(String),
}

/// Reads one leaf of `kind` from the front of `rest`. `spelling` constrains an
/// identifier when it is `Some`.
pub(crate) fn parse_leaf(
    kind: LeafKind,
    spelling: Option<&str>,
    rest: &Syntax,
    next_syntax_id: &mut usize,
) -> Result<(ParsedLeaf, usize), LeafFailure> {
    let tokens = rest.tokens();
    let first = skip_trivia(tokens, 0, tokens.len());
    let token_slice = |from: usize, to: usize| syntax_slice(rest, from, to);
    let parsed = match kind {
        LeafKind::Expr
        | LeafKind::CallExpr
        | LeafKind::CharExpr
        | LeafKind::StringExpr
        | LeafKind::UnstructuredExpr => {
            let (expression, consumed) =
                staple_syntax::parse_expression_prefix(rest, next_syntax_id)
                    .map_err(|error| LeafFailure::Parse(error.message))?;
            let accepted = match kind {
                LeafKind::CallExpr => matches!(expression, Expression::Call(_)),
                LeafKind::CharExpr => matches!(expression, Expression::Char(_)),
                LeafKind::StringExpr => matches!(expression, Expression::String(_)),
                LeafKind::UnstructuredExpr => !matches!(
                    expression,
                    Expression::Name(_) | Expression::Call(_) | Expression::VisibilityArgument(_)
                ),
                _ => true,
            };
            if !accepted {
                return Err(LeafFailure::Mismatch);
            }
            (ParsedLeaf::Expression(expression), consumed)
        }
        LeafKind::Type | LeafKind::ProductType | LeafKind::UnstructuredType => {
            let (ty, consumed) = staple_syntax::parse_type_prefix(rest, next_syntax_id)
                .map_err(|error| LeafFailure::Parse(error.message))?;
            let structured = is_structured_product_type(&ty);
            let accepted = match kind {
                LeafKind::ProductType => structured,
                LeafKind::UnstructuredType => !structured,
                _ => true,
            };
            if !accepted {
                return Err(LeafFailure::Mismatch);
            }
            (ParsedLeaf::Type(ty), consumed)
        }
        LeafKind::Pattern | LeafKind::BindingPattern | LeafKind::NominalPattern => {
            let (pattern, consumed) = staple_syntax::parse_pattern_prefix(rest, next_syntax_id)
                .map_err(|error| LeafFailure::Parse(error.message))?;
            let accepted = match kind {
                LeafKind::BindingPattern => matches!(pattern, Pattern::Binding(_)),
                LeafKind::NominalPattern => matches!(pattern, Pattern::Nominal(_)),
                _ => true,
            };
            if !accepted {
                return Err(LeafFailure::Mismatch);
            }
            (ParsedLeaf::Pattern(pattern), consumed)
        }
        LeafKind::Item
        | LeafKind::ModifiedItem
        | LeafKind::TypeDeclarationItem
        | LeafKind::UnstructuredItem => {
            let (item, consumed) = staple_syntax::parse_item_prefix(rest, next_syntax_id)
                .map_err(|error| LeafFailure::Parse(error.message))?;
            let accepted = match kind {
                LeafKind::ModifiedItem => matches!(item, Item::Modified(_)),
                LeafKind::TypeDeclarationItem => matches!(item, Item::TypeDeclaration(_)),
                LeafKind::UnstructuredItem => {
                    !matches!(item, Item::Modified(_) | Item::TypeDeclaration(_))
                }
                _ => true,
            };
            if !accepted {
                return Err(LeafFailure::Mismatch);
            }
            (ParsedLeaf::Item(item), consumed)
        }
        LeafKind::Modifier | LeafKind::ModifierCall | LeafKind::UnstructuredModifier => {
            let (modifier, consumed) = staple_syntax::parse_modifier_prefix(rest, next_syntax_id)
                .map_err(|error| LeafFailure::Parse(error.message))?;
            let accepted = match kind {
                LeafKind::ModifierCall => modifier.doc.is_none(),
                LeafKind::UnstructuredModifier => modifier.doc.is_some(),
                _ => true,
            };
            if !accepted {
                return Err(LeafFailure::Mismatch);
            }
            (ParsedLeaf::Modifier(modifier), consumed)
        }
        LeafKind::TypeBody => {
            let (body, consumed) = staple_syntax::parse_type_body_prefix(rest, next_syntax_id)
                .map_err(|error| LeafFailure::Parse(error.message))?;
            (ParsedLeaf::TypeBody(body), consumed)
        }
        LeafKind::TypeParameter => {
            let (parameter, consumed) =
                staple_syntax::parse_type_parameter_prefix(rest, next_syntax_id)
                    .map_err(|error| LeafFailure::Parse(error.message))?;
            (ParsedLeaf::TypeParameter(parameter), consumed)
        }
        LeafKind::TypeParameters => {
            let (parameters, consumed) =
                staple_syntax::parse_type_parameters_prefix(rest, next_syntax_id)
                    .map_err(|error| LeafFailure::Parse(error.message))?;
            (ParsedLeaf::TypeParameters(parameters), consumed)
        }
        LeafKind::Ident => {
            let Some(token) = tokens.get(first) else {
                return Err(LeafFailure::Mismatch);
            };
            if token.kind != TokenKind::Identifier
                || spelling.is_some_and(|expected| expected != token.text)
            {
                return Err(LeafFailure::Mismatch);
            }
            (
                ParsedLeaf::Ident(NameExpression {
                    syntax: token_slice(first, first + 1),
                    name: token.text.clone(),
                }),
                first + 1,
            )
        }
        LeafKind::Comma | LeafKind::Equals | LeafKind::FatArrow => {
            let expected = match kind {
                LeafKind::Comma => TokenKind::Comma,
                LeafKind::Equals => TokenKind::Equals,
                _ => TokenKind::FatArrow,
            };
            if tokens.get(first).is_none_or(|token| token.kind != expected) {
                return Err(LeafFailure::Mismatch);
            }
            (
                ParsedLeaf::Punctuation(kind, token_slice(first, first + 1)),
                first + 1,
            )
        }
        LeafKind::Visibility => {
            if tokens
                .get(first)
                .is_some_and(|token| token.kind == TokenKind::Pub)
            {
                let mut end = first + 1;
                let mut visibility = VisibilityKind::Public;
                let next = skip_trivia(tokens, end, tokens.len());
                if tokens
                    .get(next)
                    .is_some_and(|token| token.kind == TokenKind::LParen)
                    && let Some(close) = matching_close(tokens, next)
                {
                    let inner = tokens[next + 1..close]
                        .iter()
                        .filter(|token| !token.kind.is_trivia())
                        .collect::<Vec<_>>();
                    if matches!(inner.as_slice(), [token] if token.kind == TokenKind::Package) {
                        visibility = VisibilityKind::Package;
                        end = close + 1;
                    }
                }
                (
                    ParsedLeaf::Visibility(VisibilitySyntax {
                        syntax: token_slice(first, end),
                        kind: visibility,
                    }),
                    end,
                )
            } else {
                // Nothing written means private; nothing is consumed.
                (
                    ParsedLeaf::Visibility(private_visibility()),
                    0,
                )
            }
        }
    };
    Ok(parsed)
}

/// The syntax of a private visibility marker, which writes nothing.
pub(crate) fn private_visibility() -> VisibilitySyntax {
    VisibilitySyntax {
        syntax: Syntax::compiler(),
        kind: VisibilityKind::Private,
    }
}

/// A delimited group a `Parse` implementation reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupKind {
    Parenthesized,
    Bracketed,
    Angled,
    Braced,
}

impl GroupKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Parenthesized => "parenthesized",
            Self::Bracketed => "bracketed",
            Self::Angled => "angle-bracketed",
            Self::Braced => "braced",
        }
    }
}

/// Reads a delimited group starting at `cursor`. Returns its whole syntax, its
/// contents, and the position after the closing delimiter.
pub(crate) fn parse_group(
    kind: GroupKind,
    syntax: &Syntax,
    cursor: usize,
) -> Result<(Syntax, Syntax, usize), String> {
    let tokens = syntax.tokens();
    let (open, close_kind) = match kind {
        GroupKind::Parenthesized => (TokenKind::LParen, TokenKind::RParen),
        GroupKind::Bracketed => (TokenKind::LBracket, TokenKind::RBracket),
        GroupKind::Angled => (TokenKind::Operator, TokenKind::Operator),
        GroupKind::Braced => (TokenKind::LBrace, TokenKind::RBrace),
    };
    let first = skip_trivia(tokens, cursor, tokens.len());
    let close = (first < tokens.len() && tokens[first].kind == open)
        .then(|| {
            if kind == GroupKind::Angled {
                (tokens[first].text == "<")
                    .then(|| matching_angle_close(tokens, first))
                    .flatten()
            } else {
                matching_close(tokens, first)
            }
        })
        .flatten()
        .filter(|close| tokens[*close].kind == close_kind);
    let Some(close) = close else {
        return Err(format!(
            "expected {} syntax at {}",
            kind.name(),
            describe_position(syntax, cursor)
        ));
    };
    let source = syntax_slice(syntax, first, close + 1);
    let inner = syntax_slice(syntax, first + 1, close);
    Ok((source, inner, close + 1))
}

/// The structural atom a `SyntaxNode` parse reads, and which `SyntaxNode`
/// alternative it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyntaxNodeAtom {
    Expr,
    Comma,
    Equals,
    FatArrow,
    Parenthesized,
    Bracketed,
    Braced,
    Item,
    Type,
    Pattern,
}

impl SyntaxNodeAtom {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Self::Expr => 1,
            Self::Comma => 2,
            Self::Equals => 3,
            Self::FatArrow => 4,
            Self::Parenthesized => 5,
            Self::Bracketed => 6,
            Self::Braced => 7,
            Self::Item => 8,
            Self::Type => 9,
            Self::Pattern => 10,
        }
    }
}

/// Reads one `SyntaxNode` atom, matching what the interpreter's structural
/// value chooses: a punctuation token, a balanced group, or the first syntax
/// that parses as an expression, item, type, or pattern.
pub(crate) fn parse_syntax_node_atom(
    rest: &Syntax,
    next_syntax_id: &mut usize,
) -> Result<(SyntaxNodeAtom, Syntax, usize), LeafFailure> {
    let tokens = rest.tokens();
    let first = skip_trivia(tokens, 0, tokens.len());
    let Some(token) = tokens.get(first) else {
        return Err(LeafFailure::Mismatch);
    };
    let end = match token.kind {
        TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
            matching_close(tokens, first).ok_or(LeafFailure::Mismatch)? + 1
        }
        _ => first + 1,
    };
    let atom = syntax_slice(rest, first, end);
    let atom_tokens = atom
        .tokens()
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let single = |kind| matches!(atom_tokens.as_slice(), [token] if token.kind == kind);
    let (alternative, syntax) = if single(TokenKind::Identifier) {
        (SyntaxNodeAtom::Expr, atom)
    } else if single(TokenKind::Comma) {
        (SyntaxNodeAtom::Comma, atom)
    } else if single(TokenKind::Equals) {
        (SyntaxNodeAtom::Equals, atom)
    } else if single(TokenKind::FatArrow) {
        (SyntaxNodeAtom::FatArrow, atom)
    } else if matches!(
        atom_tokens.as_slice(),
        [open, .., close]
            if (open.kind, close.kind) == (TokenKind::LParen, TokenKind::RParen)
    ) {
        (SyntaxNodeAtom::Parenthesized, atom)
    } else if matches!(
        atom_tokens.as_slice(),
        [open, .., close]
            if (open.kind, close.kind) == (TokenKind::LBracket, TokenKind::RBracket)
    ) {
        (SyntaxNodeAtom::Bracketed, atom)
    } else if matches!(
        atom_tokens.as_slice(),
        [open, .., close]
            if (open.kind, close.kind) == (TokenKind::LBrace, TokenKind::RBrace)
    ) {
        (SyntaxNodeAtom::Braced, atom)
    } else if staple_syntax::parse_expression_fragment(&atom, next_syntax_id).is_ok() {
        (SyntaxNodeAtom::Expr, atom)
    } else if staple_syntax::parse_item_fragment(&atom, next_syntax_id).is_ok() {
        (SyntaxNodeAtom::Item, atom)
    } else if staple_syntax::parse_type_fragment(&atom, false, next_syntax_id).is_ok() {
        (SyntaxNodeAtom::Type, atom)
    } else if staple_syntax::parse_pattern_fragment(&atom, false, next_syntax_id).is_ok() {
        (SyntaxNodeAtom::Pattern, atom)
    } else {
        return Err(LeafFailure::Mismatch);
    };
    Ok((alternative, syntax, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_keeps_the_parent_token_stream() {
        let mut next = 0;
        let parent = syntax_from_text("one two three", &mut next);
        let slice = syntax_slice(&parent, 1, 2);
        assert_eq!(slice.text(), " ");
        assert_eq!(slice.tokens().len(), 1);
        assert!(slice.shares_token_stream(&parent));
        assert_eq!(slice.token_range(), 1..2);
    }

    #[test]
    fn a_position_counts_lines_and_columns() {
        let mut next = 0;
        let syntax = syntax_from_text("one\ntwo three", &mut next);
        assert_eq!(describe_position(&syntax, 0), "line 1, column 1");
        assert_eq!(describe_position(&syntax, 1), "line 2, column 1");
        assert_eq!(describe_position(&syntax, 2), "line 2, column 1");
        assert_eq!(describe_position(&syntax, 4), "line 2, column 5");
        assert_eq!(describe_position(&syntax, 5), "end of input");
    }

    #[test]
    fn matching_close_balances_nested_groups() {
        let mut next = 0;
        let syntax = syntax_from_text("(one [two] three)", &mut next);
        let tokens = syntax.tokens();
        assert_eq!(matching_close(tokens, 0), Some(tokens.len() - 1));
        let mut angle = 0;
        let syntax = syntax_from_text("<one (two)>", &mut angle);
        let tokens = syntax.tokens();
        assert_eq!(matching_angle_close(tokens, 0), Some(tokens.len() - 1));
    }
}
