use crate::ast::{SyntaxToken, TokenKind};
use crate::combinator::{Parser, tag, take_while1};

/// Losslessly tokenizes Staple source, including whitespace and comments.
///
/// Unknown and incomplete input is retained so editor tooling can continue to
/// provide useful results while a document is being edited.
pub fn lex(source: &str) -> Vec<SyntaxToken> {
    let mut tokens = Vec::new();
    let mut offset = 0;
    while offset < source.len() {
        let start = offset;

        let newline = tag("\r\n")
            .or(tag("\n"))
            .or(tag("\r"))
            .map(|_| TokenKind::Newline);
        let whitespace = take_while1(|c| matches!(c, ' ' | '\t')).map(|_| TokenKind::Whitespace);
        let comment = |text: &str, at: usize| {
            if !text.get(at..)?.starts_with("//") {
                return None;
            }
            let length = text[at..].find(['\r', '\n']).unwrap_or(text.len() - at);
            Some((TokenKind::LineComment, at + length))
        };
        let block_comment = |text: &str, at: usize| {
            lex_block_comment(text, at).map(|end| (TokenKind::BlockComment, end))
        };
        let string =
            |text: &str, at: usize| lex_string(text, at).map(|end| (TokenKind::String, end));
        let identifier = |text: &str, at: usize| {
            lex_identifier(text, at).map(|end| (TokenKind::Identifier, end))
        };
        let float = |text: &str, at: usize| lex_float(text, at).map(|end| (TokenKind::Float, end));
        let integer = take_while1(|c| c.is_ascii_digit()).map(|_| TokenKind::Integer);
        let fixed_operator = |text: &str, at: usize| {
            lex_fixed_operator(text, at).map(|end| (TokenKind::Operator, end))
        };

        let parsed = newline
            .or(whitespace)
            .or(comment)
            .or(block_comment)
            .or(string)
            .or(tag("=>").map(|_| TokenKind::FatArrow))
            .or(tag("->").map(|_| TokenKind::Arrow))
            .or(tag("...").map(|_| TokenKind::Ellipsis))
            .or(float)
            .or(identifier)
            .or(integer)
            .or(fixed_operator)
            .parse(source, offset);

        let (mut kind, end) = parsed.unwrap_or_else(|| {
            let character = source[offset..]
                .chars()
                .next()
                .expect("offset is in source");
            (
                single_character_kind(character),
                offset + character.len_utf8(),
            )
        });

        let text = &source[start..end];
        if kind == TokenKind::Identifier {
            kind = match text {
                "use" => TokenKind::Use,
                "package" => TokenKind::Package,
                "as" => TokenKind::As,
                "satisfies" => TokenKind::Satisfies,
                "pub" => TokenKind::Pub,
                "let" => TokenKind::Let,
                "mut" => TokenKind::Mut,
                "move" => TokenKind::Move,
                "signal" => TokenKind::Signal,
                "return" => TokenKind::Return,
                "loop" => TokenKind::Loop,
                "break" => TokenKind::Break,
                "continue" => TokenKind::Continue,
                "def" => TokenKind::Def,
                "const" => TokenKind::Const,
                "extern" => TokenKind::Extern,
                "type" => TokenKind::Type,
                "mod" => TokenKind::Mod,
                "companion" => TokenKind::Companion,
                "macro" => TokenKind::Macro,
                "trait" => TokenKind::Trait,
                "impl" => TokenKind::Impl,
                "match" => TokenKind::Match,
                "alias" => TokenKind::Alias,
                "opaque" => TokenKind::Opaque,
                "where" => TokenKind::Where,
                "_" => TokenKind::Underscore,
                _ => TokenKind::Identifier,
            };
        }
        tokens.push(SyntaxToken {
            kind,
            text: text.to_owned(),
            span: start..end,
            origin: None,
        });
        offset = end;
    }
    tokens
}

fn lex_float(source: &str, offset: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut end = offset;
    let mut has_dot = false;

    if bytes.get(end) == Some(&b'.') {
        if source[..offset]
            .chars()
            .next_back()
            .is_some_and(|character| {
                character == '_'
                    || character.is_alphanumeric()
                    || matches!(character, ')' | ']' | '}' | '*')
            })
        {
            return None;
        }
        if !bytes.get(end + 1).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        has_dot = true;
        end += 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
    } else if bytes.get(end).is_some_and(u8::is_ascii_digit) {
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        let exponent_after_dot = matches!(bytes.get(end + 1), Some(b'e' | b'E')) && {
            let mut exponent_digit = end + 2;
            if matches!(bytes.get(exponent_digit), Some(b'+' | b'-')) {
                exponent_digit += 1;
            }
            bytes.get(exponent_digit).is_some_and(u8::is_ascii_digit)
        };
        if bytes.get(end) == Some(&b'.')
            && bytes.get(end + 1) != Some(&b'.')
            && (exponent_after_dot
                || !source[end + 1..]
                    .chars()
                    .next()
                    .is_some_and(|character| character == '_' || character.is_alphabetic()))
        {
            has_dot = true;
            end += 1;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
        }
    } else {
        return None;
    }

    let mut has_exponent = false;
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let exponent = end;
        let mut exponent_end = end + 1;
        if matches!(bytes.get(exponent_end), Some(b'+' | b'-')) {
            exponent_end += 1;
        }
        let digits = exponent_end;
        while bytes.get(exponent_end).is_some_and(u8::is_ascii_digit) {
            exponent_end += 1;
        }
        let next_is_identifier = source[exponent_end..]
            .chars()
            .next()
            .is_some_and(|character| character == '_' || character.is_alphabetic());
        if exponent_end > digits || !next_is_identifier {
            has_exponent = true;
            end = exponent_end.max(exponent + 1);
        }
    }

    (has_dot || has_exponent).then_some(end)
}

/// Returns the byte offset just past a `/* ... */` block comment starting at
/// `offset`. Block comments nest, so an inner `/*` must be balanced by a `*/`
/// before the comment closes. An unterminated comment extends to the end of the
/// input so incomplete documents still tokenize losslessly.
fn lex_block_comment(source: &str, offset: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(offset) != Some(&b'/') || bytes.get(offset + 1) != Some(&b'*') {
        return None;
    }
    let mut cursor = offset + 2;
    let mut depth = 1usize;
    while cursor + 1 < source.len() {
        match (bytes[cursor], bytes[cursor + 1]) {
            (b'/', b'*') => {
                depth += 1;
                cursor += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                cursor += 2;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    Some(source.len())
}

fn lex_string(source: &str, offset: usize) -> Option<usize> {
    if source.as_bytes().get(offset) != Some(&b'"') {
        return None;
    }
    let mut cursor = offset + 1;
    let mut escaped = false;
    let mut interpolation_depth = 0usize;
    while cursor < source.len() {
        let character = source[cursor..].chars().next()?;
        if interpolation_depth == 0 {
            if character == '"' && !escaped {
                return Some(cursor + 1);
            }
            if character == '$'
                && !escaped
                && source[cursor + character.len_utf8()..].starts_with('{')
            {
                interpolation_depth = 1;
                cursor += character.len_utf8() + 1;
                escaped = false;
                continue;
            }
            escaped = character == '\\' && !escaped;
            if character != '\\' {
                escaped = false;
            }
            cursor += character.len_utf8();
            continue;
        }
        if character == '"' {
            cursor = lex_string(source, cursor)?;
            continue;
        }
        if source[cursor..].starts_with("//") {
            cursor = source[cursor..]
                .find(['\r', '\n'])
                .map_or(source.len(), |relative| cursor + relative);
            continue;
        }
        if source[cursor..].starts_with("/*") {
            cursor = lex_block_comment(source, cursor)?;
            continue;
        }
        match character {
            '{' => interpolation_depth += 1,
            '}' => interpolation_depth -= 1,
            _ => {}
        }
        cursor += character.len_utf8();
    }
    Some(source.len())
}

fn lex_identifier(source: &str, offset: usize) -> Option<usize> {
    let tail = source.get(offset..)?;
    let mut characters = tail.char_indices();
    let (_, first) = characters.next()?;
    if first != '_' && !first.is_alphabetic() {
        return None;
    }
    let mut end = offset + first.len_utf8();
    for (relative, character) in characters {
        if character != '_' && !character.is_alphanumeric() {
            break;
        }
        end = offset + relative + character.len_utf8();
    }
    Some(end)
}

fn lex_fixed_operator(source: &str, offset: usize) -> Option<usize> {
    let tail = source.get(offset..)?;
    [
        "..=", "&&", "||", "==", "!=", "<=", ">=", "..", "<:", "~>", "?", "|", "<", ">", "^",
    ]
    .into_iter()
    .find(|operator| tail.starts_with(operator))
    .map(|operator| offset + operator.len())
}

fn single_character_kind(character: char) -> TokenKind {
    match character {
        '(' => TokenKind::LParen,
        ')' => TokenKind::RParen,
        '{' => TokenKind::LBrace,
        '}' => TokenKind::RBrace,
        '[' => TokenKind::LBracket,
        ']' => TokenKind::RBracket,
        ':' => TokenKind::Colon,
        ',' => TokenKind::Comma,
        ';' => TokenKind::Semicolon,
        '.' => TokenKind::Dot,
        '=' => TokenKind::Equals,
        '*' => TokenKind::Star,
        '+' => TokenKind::Plus,
        '-' => TokenKind::Minus,
        '/' => TokenKind::Slash,
        '$' => TokenKind::Dollar,
        '@' => TokenKind::At,
        '!' => TokenKind::Bang,
        _ => TokenKind::Unknown,
    }
}
