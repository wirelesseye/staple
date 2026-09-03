//! Canonical, parse-only formatting for Staple source.
//!
//! Formatting deliberately operates on the lossless token stream. In
//! particular, macro arguments may be opaque to the parser and macros can
//! observe their punctuation, so this module changes trivia but never invents
//! or removes non-trivia tokens.

use crate::{ParseError, SyntaxToken, TokenKind, lex, parse};

const WIDTH: usize = 100;
const INDENT: &str = "    ";

/// Parses and formats one complete Staple source file.
pub fn format_source(source: &str) -> Result<String, ParseError> {
    parse(source)?;
    let tokens = lex(source);
    let output = format_token_stream(&tokens);
    // This is intentionally a runtime invariant: a formatter must never emit
    // source the parser itself rejects.
    parse(&output)?;
    Ok(output)
}

/// Formats an already-tokenized source fragment without requiring it to be a
/// complete, independently parseable module. Macro expansion uses this for
/// reconstructed output which may retain documented splice placeholders.
pub(crate) fn format_token_stream(tokens: &[SyntaxToken]) -> String {
    let mut formatter = Formatter::new(tokens);
    formatter.format();
    formatter.finish()
}

#[derive(Clone, Copy)]
struct Group {
    close: usize,
    multiline: bool,
}

struct Formatter<'a> {
    tokens: &'a [SyntaxToken],
    groups: Vec<Option<Group>>,
    output: String,
    line: String,
    indent: usize,
    pending_newlines: usize,
    at_line_start: bool,
    previous: Option<TokenKind>,
    separated: bool,
    source_indent: Option<usize>,
}

impl<'a> Formatter<'a> {
    fn new(tokens: &'a [SyntaxToken]) -> Self {
        Self {
            tokens,
            groups: groups(tokens),
            output: String::new(),
            line: String::new(),
            indent: 0,
            pending_newlines: 0,
            at_line_start: true,
            previous: None,
            separated: false,
            source_indent: None,
        }
    }

    fn format(&mut self) {
        for (index, token) in self.tokens.iter().enumerate() {
            match token.kind {
                TokenKind::Whitespace => {
                    self.separated = true;
                    if self.pending_newlines > 0 {
                        let columns = token.text.chars().fold(0usize, |column, character| {
                            if character == '\t' {
                                (column / 4 + 1) * 4
                            } else {
                                column + 1
                            }
                        });
                        self.source_indent = Some(columns.div_ceil(4));
                    }
                }
                TokenKind::Newline => {
                    self.pending_newlines = (self.pending_newlines + 1).min(2);
                    self.separated = true;
                    self.source_indent = Some(0);
                }
                TokenKind::LineComment => {
                    self.flush_pending(true);
                    if !self.at_line_start {
                        self.space();
                    }
                    self.text(&token.text);
                    self.hardline();
                }
                TokenKind::BlockComment => {
                    self.flush_pending(false);
                    if self.separated || needs_space(self.previous, token.kind) {
                        self.space();
                    }
                    self.block_comment(&token.text);
                }
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => {
                    self.flush_pending(false);
                    if self.separated || needs_space(self.previous, token.kind) {
                        self.space();
                    }
                    self.text(&token.text);
                    let multiline = self.groups[index].is_some_and(|group| group.multiline);
                    if multiline {
                        self.indent += 1;
                        self.hardline();
                    }
                }
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    let opening = matching_open(self.tokens, index);
                    let multiline = opening
                        .and_then(|opening| self.groups[opening])
                        .is_some_and(|group| group.multiline);
                    if multiline {
                        self.indent = self.indent.saturating_sub(1);
                        self.hardline();
                    } else {
                        self.pending_newlines = 0;
                    }
                    self.trim_line();
                    if self.separated && token.kind == TokenKind::RBrace && !multiline {
                        self.space();
                    }
                    self.text(&token.text);
                }
                TokenKind::Comma => {
                    self.pending_newlines = 0;
                    self.trim_line();
                    self.text(",");
                    if containing_group(&self.groups, index).is_some_and(|group| group.multiline) {
                        self.hardline();
                    } else {
                        self.space();
                    }
                }
                TokenKind::Semicolon => {
                    self.pending_newlines = 0;
                    self.trim_line();
                    self.text(";");
                    if containing_group(&self.groups, index).is_some_and(|group| group.multiline) {
                        self.hardline();
                    } else {
                        self.space();
                    }
                }
                TokenKind::Dot => {
                    self.pending_newlines = 0;
                    self.trim_line();
                    self.text(".");
                }
                TokenKind::Colon => {
                    self.pending_newlines = 0;
                    self.trim_line();
                    self.text(":");
                    self.space();
                }
                TokenKind::Operator
                | TokenKind::Equals
                | TokenKind::Arrow
                | TokenKind::FatArrow => {
                    self.flush_pending(false);
                    self.space();
                    self.text(&token.text);
                    self.space();
                }
                _ => {
                    self.flush_pending(true);
                    if self.separated || needs_space(self.previous, token.kind) {
                        self.space();
                    }
                    if self.current_column() + token.text.len() > WIDTH && !self.at_line_start {
                        self.hardline();
                    }
                    self.text(&token.text);
                }
            }
            if !token.kind.is_trivia() {
                self.previous = Some(token.kind);
                self.separated = false;
                self.source_indent = None;
            }
        }
    }

    fn block_comment(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for (index, part) in normalized.split('\n').enumerate() {
            if index > 0 {
                self.hardline();
            }
            self.text(part);
        }
    }

    fn flush_pending(&mut self, allow_blank: bool) {
        if self.pending_newlines == 0 {
            return;
        }
        let count = if allow_blank && self.pending_newlines > 1 {
            2
        } else {
            1
        };
        self.hardline();
        if count == 2 && !self.output.is_empty() && !self.output.ends_with("\n\n") {
            self.output.push('\n');
        }
        self.pending_newlines = 0;
    }

    fn text(&mut self, text: &str) {
        if self.at_line_start {
            self.line.push_str(
                &INDENT.repeat(self.source_indent.unwrap_or(self.indent).max(self.indent)),
            );
            self.at_line_start = false;
        }
        self.line.push_str(text);
    }

    fn space(&mut self) {
        if !self.at_line_start && !self.line.ends_with(char::is_whitespace) {
            self.line.push(' ');
        }
    }

    fn trim_line(&mut self) {
        self.line.truncate(self.line.trim_end().len());
    }

    fn hardline(&mut self) {
        self.trim_line();
        if !self.line.is_empty() {
            self.output.push_str(&self.line);
        }
        if !self.output.is_empty() && !self.output.ends_with('\n') || !self.line.is_empty() {
            self.output.push('\n');
        }
        self.line.clear();
        self.at_line_start = true;
        self.pending_newlines = 0;
    }

    fn current_column(&self) -> usize {
        self.line.chars().count()
    }

    fn finish(mut self) -> String {
        self.trim_line();
        if !self.line.is_empty() {
            self.output.push_str(&self.line);
        }
        while self.output.ends_with("\n\n") {
            self.output.pop();
        }
        if !self.output.is_empty() && !self.output.ends_with('\n') {
            self.output.push('\n');
        }
        self.output
    }
}

fn groups(tokens: &[SyntaxToken]) -> Vec<Option<Group>> {
    let mut result = vec![None; tokens.len()];
    let mut stack = Vec::<usize>::new();
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => stack.push(index),
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                let Some(open) = stack.pop() else { continue };
                let source_multiline = tokens[open + 1..index]
                    .iter()
                    .any(|token| token.kind == TokenKind::Newline);
                let line_comment = tokens[open + 1..index]
                    .iter()
                    .any(|token| token.kind == TokenKind::LineComment);
                let top_level_separator = has_top_level_separator(tokens, open + 1, index);
                let width = tokens[open..=index]
                    .iter()
                    .filter(|token| !token.kind.is_trivia())
                    .map(|token| token.text.len() + 1)
                    .sum::<usize>();
                result[open] = Some(Group {
                    close: index,
                    multiline: source_multiline
                        || line_comment
                        || top_level_separator
                        || width > WIDTH,
                });
            }
            _ => {}
        }
    }
    result
}

fn has_top_level_separator(tokens: &[SyntaxToken], start: usize, end: usize) -> bool {
    let mut depth = 0usize;
    for token in &tokens[start..end] {
        match token.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                depth = depth.saturating_sub(1)
            }
            TokenKind::Semicolon if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn matching_open(tokens: &[SyntaxToken], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for index in (0..close).rev() {
        match tokens[index].kind {
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth += 1,
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket if depth == 0 => {
                return Some(index);
            }
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth -= 1,
            _ => {}
        }
    }
    None
}

fn containing_group(groups: &[Option<Group>], index: usize) -> Option<Group> {
    groups[..index]
        .iter()
        .flatten()
        .rev()
        .copied()
        .find(|group| group.close > index)
}

fn needs_space(previous: Option<TokenKind>, current: TokenKind) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if matches!(
        current,
        TokenKind::RParen
            | TokenKind::RBrace
            | TokenKind::RBracket
            | TokenKind::Comma
            | TokenKind::Semicolon
            | TokenKind::Dot
            | TokenKind::Colon
    ) {
        return false;
    }
    if matches!(
        previous,
        TokenKind::LParen
            | TokenKind::LBrace
            | TokenKind::LBracket
            | TokenKind::Dot
            | TokenKind::Dollar
            | TokenKind::At
    ) {
        return false;
    }
    !matches!(current, TokenKind::LParen | TokenKind::LBracket)
}

#[cfg(test)]
mod tests {
    use super::{format_source, format_token_stream};
    use crate::{TokenKind, lex};

    #[test]
    fn formats_uniform_macro_applications_without_resolving_them() {
        let source =
            "macro choose=value: Expr=>value\nlet result=choose {\nready=>one,\nelse=>two,\n}\n";
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("macro choose = value: Expr => value"));
        assert!(formatted.contains("choose {\n    ready => one,"));
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn preserves_opaque_trailing_separator_state() {
        let without = format_source("let value = unknown { first => 1 }\n").unwrap();
        let with = format_source("let value = unknown { first => 1, }\n").unwrap();
        assert!(!without.contains("1,"));
        assert!(with.contains("1,"));
    }

    #[test]
    fn normalizes_comments_and_newlines() {
        let formatted = format_source("let x=1  // keep me\r\n\r\n\r\nlet y=2").unwrap();
        assert_eq!(formatted, "let x = 1 // keep me\n\nlet y = 2\n");
    }

    #[test]
    fn token_formatting_does_not_require_standalone_parseable_source() {
        let source = "type alias $name=$ty\n";
        assert!(format_source(source).is_err());
        let before = lex(source)
            .into_iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| (token.kind, token.text))
            .collect::<Vec<_>>();
        let formatted = format_token_stream(&lex(source));
        let after = lex(&formatted)
            .into_iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| (token.kind, token.text))
            .collect::<Vec<_>>();
        assert_eq!(before, after);
        assert_eq!(formatted, "type alias $name = $ty\n");
        assert!(before.iter().any(|(kind, _)| *kind == TokenKind::Dollar));
    }
}
