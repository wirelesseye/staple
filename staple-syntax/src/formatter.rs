//! Canonical, parse-only formatting for Staple source.
//!
//! Formatting deliberately operates on the lossless token stream. In
//! particular, macro arguments may be opaque to the parser and macros can
//! observe their punctuation, so this module changes trivia but never invents
//! or removes non-trivia tokens.

use crate::parser::{column_within, statement_starts};
use crate::{ParseError, SyntaxToken, TokenKind, lex, parse};

const WIDTH: usize = 100;
const INDENT: &str = "    ";

/// Parses and formats one complete Staple source file.
pub fn format_source(source: &str) -> Result<String, ParseError> {
    parse(source)?;
    let starts = statement_starts(source)?;
    let tokens = lex(source);
    let brackets = crate::parser::type_argument_brackets(source)?;
    let output = format_with_statement_starts(&tokens, &starts, &brackets);
    // These are intentionally runtime invariants: a formatter must never emit
    // source the parser itself rejects, nor move a statement boundary.
    parse(&output)?;
    let output_starts = statement_starts(&output)?;
    let output_tokens = lex(&output);
    let texts = |tokens: &[SyntaxToken], starts: &[usize]| {
        starts
            .iter()
            .map(|index| (non_trivia_ordinal(tokens, *index), tokens[*index].text.clone()))
            .collect::<Vec<_>>()
    };
    if texts(&tokens, &starts) != texts(&output_tokens, &output_starts) {
        return Err(ParseError {
            offset: 0,
            location: crate::SourceLocation { line: 1, column: 1 },
            message: "formatting would change where statements begin".to_owned(),
        });
    }
    Ok(output)
}

fn non_trivia_ordinal(tokens: &[SyntaxToken], index: usize) -> usize {
    tokens[..index]
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .count()
}

/// Formats an already-tokenized source fragment without requiring it to be a
/// complete, independently parseable module. Macro expansion uses this for
/// reconstructed output which may retain documented splice placeholders.
pub fn format_token_stream(tokens: &[SyntaxToken]) -> String {
    let source = tokens
        .iter()
        .map(|token| token.text.as_str())
        .collect::<String>();
    // Prefer the parser's statements; guess only for streams that do not parse.
    let starts = statement_starts(&source)
        .ok()
        .filter(|_| {
            let relexed = lex(&source);
            relexed.len() == tokens.len()
                && relexed
                    .iter()
                    .zip(tokens)
                    .all(|(relexed, token)| relexed.kind == token.kind)
        })
        .unwrap_or_else(|| guess_statement_starts(tokens));
    let brackets = crate::parser::type_argument_brackets(&source)
        .ok()
        .filter(|_| lex(&source).len() == tokens.len())
        .unwrap_or_else(|| guess_type_brackets(tokens));
    format_with_statement_starts(tokens, &starts, &brackets)
}

/// Guesses which `<` and `>` tokens delimit type arguments when the stream does
/// not parse: a `<` touching the name before it, with a matching `>` before the
/// end of its line.
fn guess_type_brackets(tokens: &[SyntaxToken]) -> Vec<usize> {
    let mut brackets = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Operator
            || token.text != "<"
            || index == 0
            || !matches!(tokens[index - 1].kind, TokenKind::Identifier | TokenKind::RBrace)
        {
            continue;
        }
        let mut depth = 0usize;
        for (offset, candidate) in tokens[index..].iter().enumerate() {
            match (candidate.kind, candidate.text.as_str()) {
                (TokenKind::Newline, _) => break,
                (TokenKind::Operator, "<") => depth += 1,
                (TokenKind::Operator, ">") => {
                    depth -= 1;
                    if depth == 0 {
                        brackets.extend([index, index + offset]);
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    brackets
}

fn format_with_statement_starts(
    tokens: &[SyntaxToken],
    starts: &[usize],
    brackets: &[usize],
) -> String {
    let mut formatter = Formatter::new(tokens, starts, brackets);
    formatter.format();
    formatter.finish()
}

/// How each token relates to the newline-separated statements around it.
struct Layout {
    /// The token begins a statement or member.
    starts: Vec<bool>,
    /// The token begins a line indented past its statement's first token,
    /// so the line continues that statement.
    continuations: Vec<bool>,
    /// The token is the first non-trivia token on its line.
    line_starts: Vec<bool>,
}

impl Layout {
    fn new(tokens: &[SyntaxToken], statement_starts: &[usize]) -> Self {
        let mut starts = vec![false; tokens.len()];
        for index in statement_starts {
            if let Some(start) = starts.get_mut(*index) {
                *start = true;
            }
        }
        let mut continuations = vec![false; tokens.len()];
        let mut line_starts = vec![false; tokens.len()];
        // Each bracket level remembers the statement its lines are measured
        // against, and whether that statement belongs to this level.
        let mut levels = vec![(None::<usize>, true)];
        let mut at_line_start = true;
        for (index, token) in tokens.iter().enumerate() {
            if token.kind == TokenKind::Newline {
                at_line_start = true;
                continue;
            }
            if token.kind.is_trivia() {
                continue;
            }
            line_starts[index] = at_line_start;
            let level = levels.last_mut().expect("root level");
            if starts[index] {
                *level = (Some(index), true);
            } else if at_line_start && let (Some(anchor), true) = *level {
                continuations[index] =
                    column_within(tokens, index).0 > column_within(tokens, anchor).0;
            }
            at_line_start = false;
            match token.kind {
                TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => {
                    let anchor = levels.last().expect("root level").0;
                    levels.push((anchor, false));
                }
                TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket if levels.len() > 1 => {
                    levels.pop();
                }
                _ => {}
            }
        }
        Self {
            starts,
            continuations,
            line_starts,
        }
    }

    /// Whether a line break before `index` must survive formatting: it
    /// separates statements or begins a continuation line.
    fn needs_break(&self, index: usize, first_in_group: bool) -> bool {
        self.line_starts[index]
            && (self.continuations[index] || (self.starts[index] && !first_in_group))
    }
}

/// Approximates the parser's statement starts for token streams that may not
/// parse: the first token of the stream, of each brace group and after each
/// `;`, and each line start not indented past the current statement.
fn guess_statement_starts(tokens: &[SyntaxToken]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut levels = vec![None::<usize>];
    let mut expect_start = true;
    let mut at_line_start = true;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind == TokenKind::Newline {
            at_line_start = true;
            continue;
        }
        if token.kind.is_trivia() {
            continue;
        }
        let level = levels.last_mut().expect("root level");
        let dedented = at_line_start
            && level.is_some_and(|anchor| {
                column_within(tokens, index).0 <= column_within(tokens, anchor).0
            });
        if expect_start || dedented {
            *level = Some(index);
            starts.push(index);
        }
        expect_start = false;
        at_line_start = false;
        match token.kind {
            TokenKind::LBrace => {
                levels.push(None);
                expect_start = true;
            }
            TokenKind::LParen | TokenKind::LBracket => levels.push(None),
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket if levels.len() > 1 => {
                levels.pop();
            }
            TokenKind::Semicolon => expect_start = true,
            _ => {}
        }
    }
    starts
}

#[derive(Clone, Copy)]
struct Group {
    close: usize,
    multiline: bool,
}

struct Formatter<'a> {
    tokens: &'a [SyntaxToken],
    layout: Layout,
    groups: Vec<Option<Group>>,
    /// Per open bracket level, whether its current statement has continued
    /// onto an indented line; each such level adds one indent.
    continuing: Vec<bool>,
    output: String,
    line: String,
    indent: usize,
    pending_newlines: usize,
    at_line_start: bool,
    previous: Option<TokenKind>,
    separated: bool,
    generic_depth: usize,
    generic_open: bool,
    /// Tokens that open or close type arguments, written without spaces.
    type_brackets: Vec<bool>,
    /// Open type-argument brackets; commas inside them never break the line.
    type_depth: usize,
}

impl<'a> Formatter<'a> {
    fn new(tokens: &'a [SyntaxToken], statement_starts: &[usize], brackets: &[usize]) -> Self {
        let layout = Layout::new(tokens, statement_starts);
        let mut type_brackets = vec![false; tokens.len()];
        for index in brackets {
            if let Some(flag) = type_brackets.get_mut(*index) {
                *flag = true;
            }
        }
        Self {
            type_brackets,
            type_depth: 0,
            tokens,
            groups: groups(tokens, &layout),
            layout,
            continuing: vec![false],
            output: String::new(),
            line: String::new(),
            indent: 0,
            pending_newlines: 0,
            at_line_start: true,
            previous: None,
            separated: false,
            generic_depth: 0,
            generic_open: false,
        }
    }

    fn format(&mut self) {
        for (index, token) in self.tokens.iter().enumerate() {
            let flat_group =
                containing_group(&self.groups, index).is_some_and(|group| !group.multiline);
            // Newlines inside a group that fits are soft layout opportunities,
            // not a reason to preserve the source's line shape.
            if !token.kind.is_trivia()
                && containing_group(&self.groups, index).is_some_and(|group| !group.multiline)
            {
                self.pending_newlines = 0;
            }
            self.track_continuation(index);
            // A `?` after `let`, `where` or `,` marks the following pattern or
            // bound; anywhere else it is the postfix operator.
            let prefix_question = token.kind == TokenKind::Operator
                && token.text == "?"
                && matches!(
                    self.previous,
                    Some(TokenKind::Let | TokenKind::Where | TokenKind::Comma)
                );
            match token.kind {
                TokenKind::Whitespace => {
                    if self.pending_newlines == 0 {
                        self.separated = true;
                    }
                }
                TokenKind::Newline => {
                    self.pending_newlines = (self.pending_newlines + 1).min(2);
                    self.separated = !flat_group;
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
                    let arrow_effect_set = token.kind == TokenKind::LBrace
                        && index > 0
                        && matches!(
                            self.tokens[index - 1].kind,
                            TokenKind::Arrow | TokenKind::FatArrow
                        )
                        && self.tokens[index - 1].span.end == token.span.start;
                    // `Name{E}` supplies an effect parameter only while the
                    // brace touches the name, so that adjacency is kept.
                    let effect_parameter = token.kind == TokenKind::LBrace
                        && index > 0
                        && self.tokens[index - 1].kind == TokenKind::Identifier;
                    if arrow_effect_set || effect_parameter || self.generic_open {
                        self.trim_line();
                    } else if self.separated || needs_space(self.previous, token.kind) {
                        self.space();
                    }
                    self.text(&token.text);
                    self.continuing.push(false);
                    let multiline = self.groups[index].is_some_and(|group| group.multiline);
                    if multiline {
                        self.indent += 1;
                        self.hardline();
                    }
                }
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    if self.continuing.len() > 1 {
                        self.continuing.pop();
                    }
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
                    if self.type_depth == 0
                        && self.generic_depth == 0
                        && containing_group(&self.groups, index).is_some_and(|group| group.multiline)
                    {
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
                TokenKind::Operator if self.type_brackets[index] && token.text == "<" => {
                    self.flush_pending(false);
                    self.trim_line();
                    self.text("<");
                    self.type_depth += 1;
                    self.generic_open = true;
                }
                TokenKind::Operator if self.type_brackets[index] => {
                    self.pending_newlines = 0;
                    self.trim_line();
                    self.text(">");
                    self.type_depth = self.type_depth.saturating_sub(1);
                }
                TokenKind::Operator
                    if token.text == "<"
                        && (self.at_line_start
                            || self.pending_newlines > 0
                            || matches!(
                                self.previous,
                                Some(
                                    TokenKind::Colon
                                        | TokenKind::Equals
                                        | TokenKind::LParen
                                        | TokenKind::Comma
                                        | TokenKind::DoubleColon
                                        | TokenKind::FatArrow
                                        | TokenKind::Arrow
                                )
                            )) =>
                {
                    self.flush_pending(false);
                    if self.separated || needs_space(self.previous, token.kind) {
                        self.space();
                    }
                    self.text("<");
                    self.generic_depth += 1;
                    self.generic_open = true;
                }
                TokenKind::Operator if token.text == ">" && self.generic_depth > 0 => {
                    self.pending_newlines = 0;
                    self.trim_line();
                    self.text(">");
                    self.space();
                    self.generic_depth -= 1;
                }
                TokenKind::Operator if prefix_question => {
                    // `let ?Pattern` and `where ?Sized T`: attached to what follows.
                    self.flush_pending(false);
                    self.space();
                    self.text("?");
                    self.generic_open = true;
                }
                TokenKind::Caret
                | TokenKind::Operator
                | TokenKind::As
                | TokenKind::DoubleColon
                | TokenKind::Equals
                | TokenKind::Arrow
                | TokenKind::FatArrow
                | TokenKind::Percent => {
                    self.flush_pending(false);
                    self.space();
                    self.text(&token.text);
                    self.space();
                }
                _ => {
                    self.flush_pending(true);
                    if self.generic_open {
                        self.trim_line();
                    } else if self.separated || needs_space(self.previous, token.kind) {
                        self.space();
                    }
                    self.text(&token.text);
                }
            }
            if !token.kind.is_trivia() {
                if token.text != "<" && !prefix_question {
                    self.generic_open = false;
                }
                self.previous = Some(token.kind);
                self.separated = false;
            }
        }
    }

    /// Updates whether the current statement has continued onto an indented
    /// line. A comment takes the state of the token it precedes, so a comment
    /// above a new statement is not indented as a continuation.
    fn track_continuation(&mut self, index: usize) {
        let token = self.tokens[index].kind;
        let subject = if !token.is_trivia() {
            index
        } else if matches!(token, TokenKind::LineComment | TokenKind::BlockComment) {
            match self.tokens[index..]
                .iter()
                .position(|token| !token.kind.is_trivia())
            {
                Some(offset) => index + offset,
                None => return,
            }
        } else {
            return;
        };
        let continuing = self.continuing.last_mut().expect("root level");
        if self.layout.starts[subject] {
            *continuing = false;
        }
        if self.layout.continuations[subject] {
            *continuing = true;
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
            let continuing = self.continuing.iter().filter(|level| **level).count();
            self.line
                .push_str(&INDENT.repeat(self.indent + continuing));
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

fn groups(tokens: &[SyntaxToken], layout: &Layout) -> Vec<Option<Group>> {
    let mut result = vec![None; tokens.len()];
    let mut stack = Vec::<usize>::new();
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => stack.push(index),
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                let Some(open) = stack.pop() else { continue };
                let line_comment = tokens[open + 1..index]
                    .iter()
                    .any(|token| token.kind == TokenKind::LineComment);
                let top_level_separator = tokens[open].kind == TokenKind::LBrace
                    && has_top_level_separator(tokens, open + 1, index);
                let statement_break = has_statement_break(tokens, layout, open + 1, index);
                let width = tokens[open..=index]
                    .iter()
                    .filter(|token| !token.kind.is_trivia())
                    .map(|token| token.text.chars().count() + 1)
                    .sum::<usize>();
                result[open] = Some(Group {
                    close: index,
                    multiline: line_comment
                        || top_level_separator
                        || statement_break
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

/// Whether a line break directly inside `start..end` separates statements or
/// begins a continuation line, so the group cannot be joined onto one line.
fn has_statement_break(tokens: &[SyntaxToken], layout: &Layout, start: usize, end: usize) -> bool {
    let mut depth = 0usize;
    let mut first_start = true;
    for index in start..end {
        let direct = depth == 0;
        match tokens[index].kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                depth = depth.saturating_sub(1)
            }
            _ => {}
        }
        if !direct {
            continue;
        }
        if layout.needs_break(index, first_start) {
            return true;
        }
        first_start &= !layout.starts[index];
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
    fn formats_generic_brackets_and_preserves_comparisons() {
        let source = "def f: <T where Copy T> T -> T = x => x\nlet less = x < y\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
        let effect = "def f: <effect E> (() ->{E} ()) ->{E} () = f => f ()\n";
        assert_eq!(format_source(effect).unwrap(), effect);
    }

    #[test]
    fn formats_repeated_elements() {
        let formatted = format_source("let cells:(I32^3)=(0^3)\n").unwrap();
        assert_eq!(formatted, "let cells: (I32 ^ 3) = (0 ^ 3)\n");
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_the_remainder_operator() {
        let source = "let wrapped=index%length\nlet scaled=a*b%c\n";
        let formatted = format_source(source).expect("remainder syntax should format");
        assert_eq!(
            formatted,
            "let wrapped = index % length\nlet scaled = a * b % c\n"
        );
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_conversion_syntax_and_import_aliases() {
        let source = "use std.convert.Convert as Convert\nlet wide=(1+2)as I64*3\nlet pair=value as(I32,String)\n";
        let formatted = format_source(source).expect("conversion syntax should format");
        assert_eq!(
            formatted,
            "use std.convert.Convert as Convert\nlet wide = (1 + 2) as I64 * 3\nlet pair = value as (I32, String)\n"
        );
        assert_eq!(format_source(&formatted).unwrap(), formatted);
        crate::parse(&formatted).expect("formatted conversions should parse");
    }

    #[test]
    fn formats_type_ascriptions_as_low_precedence_operators() {
        let source = "let value=I32::a+b\nlet identity=I32 -> I32::value=>value\n";
        let formatted = format_source(source).expect("type ascriptions should format");
        assert_eq!(
            formatted,
            "let value = I32 :: a + b\nlet identity = I32 -> I32 :: value => value\n"
        );
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_uniform_macro_applications_without_resolving_them() {
        let source =
            "macro choose=value: Expr=>value\nlet result=choose {\nready=>one,\nelse=>two,\n}\n";
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("macro choose = value: Expr => value"));
        assert!(formatted.contains("choose {ready => one, else => two,}"));
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_juxtaposed_macro_parameters() {
        let source = "macro choose=(condition: Expr;when_true: Expr;_: Ident<\"else\">;when_false: Expr)=>quote {}\n";
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains(
            "macro choose = (condition: Expr; when_true: Expr; _: Ident<\"else\">; when_false: Expr) => quote {}"
        ));
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
    fn canonicalizes_indentation_and_compacts_groups_that_fit() {
        let formatted =
            format_source("let value = {\n        (\n        1,\n        2,\n    )\n}\n").unwrap();
        assert_eq!(formatted, "let value = {(1, 2,)}\n");
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn indents_continuation_lines_and_keeps_statement_lines() {
        let source = concat!(
            "def f = () => { let a = 1\n",
            "                a }\n",
            "let total = 1\n",
            "  + 2\n",
            "      + 3\n",
            "// leads the next statement\n",
            "let call = f a\n",
            "  b {\n",
            "x\n",
            "      y\n",
            "}\n",
        );
        let formatted = format_source(source).unwrap();
        assert_eq!(
            formatted,
            concat!(
                "def f = () => {\n",
                "    let a = 1\n",
                "    a\n",
                "}\n",
                "let total = 1\n",
                "    + 2\n",
                "    + 3\n",
                "// leads the next statement\n",
                "let call = f a\n",
                "    b {\n",
                "        x\n",
                "            y\n",
                "    }\n",
            )
        );
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn token_stream_formatting_guesses_continuations() {
        let source = "let total = $value\n  + 2\nlet next = 3\n";
        assert_eq!(
            format_token_stream(&lex(source)),
            "let total = $value\n    + 2\nlet next = 3\n"
        );
    }

    #[test]
    fn token_formatting_does_not_require_standalone_parseable_source() {
        let source = "type $name = $ty\n";
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
        assert_eq!(formatted, "type $name = $ty\n");
        assert!(before.iter().any(|(kind, _)| *kind == TokenKind::Dollar));
    }
}

#[cfg(test)]
mod parameter_product_tests {
    use super::format_source;
    #[test]
    fn preserves_slotted_type_arguments_and_tight_indices() {
        let source = "type Add=Callable<(I32;I32), I32>\ndef add:(I32;I32)->I32=(x;y)=>x+y\nlet value=values[index]\n";
        let formatted = format_source(source).expect("slotted products format");
        assert!(formatted.contains("Callable<(I32; I32), I32>"), "{formatted}");
        assert!(formatted.contains("values[index]"), "{formatted}");
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }
}

#[cfg(test)]
mod effect_adjacency_tests {
    use super::format_source;

    #[test]
    fn preserves_effect_arrow_adjacency_and_spaced_blocks() {
        let source = "def f:()->{IO}()=()=>{IO} { () }\ndef pure=()=>{} ()\ndef block=()=> { () }\ndef commented=()=> /* block */ { () }\n";
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("->{IO}"), "{formatted}");
        assert!(formatted.contains("=>{IO}"), "{formatted}");
        assert!(formatted.contains("=>{} ()"), "{formatted}");
        assert!(formatted.contains("=> { () }"), "{formatted}");
        assert!(formatted.contains("=> /* block */ { () }"), "{formatted}");
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }
}
