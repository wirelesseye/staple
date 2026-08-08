use crate::ast::{SyntaxToken, TokenKind};
use crate::combinator::{Parser, tag, take_while1};

pub(crate) fn lex(source: &str) -> Vec<SyntaxToken> {
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
        let string =
            |text: &str, at: usize| lex_string(text, at).map(|end| (TokenKind::String, end));
        let identifier = |text: &str, at: usize| {
            lex_identifier(text, at).map(|end| (TokenKind::Identifier, end))
        };
        let integer = take_while1(|c| c.is_ascii_digit()).map(|_| TokenKind::Integer);

        let parsed = newline
            .or(whitespace)
            .or(comment)
            .or(string)
            .or(tag("=>").map(|_| TokenKind::FatArrow))
            .or(tag("->").map(|_| TokenKind::Arrow))
            .or(tag("...").map(|_| TokenKind::Ellipsis))
            .or(identifier)
            .or(integer)
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
                "let" => TokenKind::Let,
                "def" => TokenKind::Def,
                "extern" => TokenKind::Extern,
                "type" => TokenKind::Type,
                "alias" => TokenKind::Alias,
                "const" => TokenKind::Const,
                "i32" => TokenKind::I32,
                "bool" => TokenKind::Bool,
                "_" => TokenKind::Underscore,
                _ => TokenKind::Identifier,
            };
        }
        tokens.push(SyntaxToken {
            kind,
            text: text.to_owned(),
            span: start..end,
        });
        offset = end;
    }
    tokens
}

fn lex_string(source: &str, offset: usize) -> Option<usize> {
    if source.as_bytes().get(offset) != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    for (relative, character) in source[offset + 1..].char_indices() {
        if character == '"' && !escaped {
            return Some(offset + 1 + relative + 1);
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
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

fn single_character_kind(character: char) -> TokenKind {
    match character {
        '(' => TokenKind::LParen,
        ')' => TokenKind::RParen,
        '{' => TokenKind::LBrace,
        '}' => TokenKind::RBrace,
        ':' => TokenKind::Colon,
        ',' => TokenKind::Comma,
        '.' => TokenKind::Dot,
        '=' => TokenKind::Equals,
        '*' => TokenKind::Star,
        '+' => TokenKind::Plus,
        '-' => TokenKind::Minus,
        '/' => TokenKind::Slash,
        _ => TokenKind::Unknown,
    }
}
