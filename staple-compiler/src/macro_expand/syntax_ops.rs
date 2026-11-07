//! Syntax operations shared by the macro interpreter and the syntax runtime.
//!
//! Everything here works on `Syntax` and AST nodes only. The interpreter calls
//! these helpers while parsing and filling quotations, and compiled macro code
//! reaches the same behaviour through the host intrinsics.

use std::collections::HashMap;
use std::sync::Arc;

use staple_syntax::{Item, Span, Syntax, SyntaxId, SyntaxToken, TokenKind, Type};

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
        Item::RepeatedItemSplice(value) => &value.syntax,
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
