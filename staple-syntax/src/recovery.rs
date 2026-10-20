//! Parsing source that does not parse, for editor tooling.
//!
//! Recovery works on the text rather than inside the grammar: it overwrites
//! the smallest broken region with spaces and parses again, until the source
//! parses. Newlines are kept and every byte becomes one space, so offsets,
//! lines and columns in the recovered tree match the original source, and a
//! source that parses is parsed exactly as [`crate::parse`] would.
//!
//! Regions are indentation units: a line together with the lines indented
//! past it and the closing-bracket lines at its own indentation, such as a
//! statement with its continuation lines or a declaration with its body. An
//! innermost unit with unbalanced brackets is discarded first. Otherwise the
//! unit holding the parse error is, widening to its enclosing unit while the
//! error stays put. An error at the start of a unit may be the previous one
//! left incomplete, which is discarded instead if that gets the parse past
//! the error. An error no unit holds, as at the end of the source, discards
//! the first top-level unit whose removal changes it.

use crate::ast::{Module, ParseError, TokenKind};
use crate::lexer::lex;
use crate::parser::parse;

/// Parse attempts before recovery gives up.
const ATTEMPTS: usize = 64;

/// The result of [`parse_recovering`].
#[derive(Debug, Clone)]
pub struct RecoveredParse {
    /// The parsed module, unless no region recovery could discard left a
    /// source that parses.
    pub module: Option<Module>,
    /// The source `module` was parsed from: the input with every discarded
    /// region overwritten by spaces.
    pub source: String,
    /// The error parsing the input reported, if it did not parse.
    pub error: Option<ParseError>,
}

/// Parses `source`, discarding the regions that keep it from parsing; see the
/// module documentation.
pub fn parse_recovering(source: &str) -> RecoveredParse {
    let error = match parse(source) {
        Ok(module) => {
            return RecoveredParse {
                module: Some(module),
                source: source.to_owned(),
                error: None,
            };
        }
        Err(error) => error,
    };
    let mut text = source.to_owned();
    let mut current = error.clone();
    let mut previous_offset = None;
    let mut widening = 0;
    for _ in 0..ATTEMPTS {
        widening = match previous_offset == Some(current.offset) {
            true => widening + 1,
            false => 0,
        };
        previous_offset = Some(current.offset);
        let lines = Lines::new(&text);
        let unbalanced = lines.innermost_unbalanced_units();
        if unbalanced.is_empty()
            && widening == 0
            && let Some(line) = lines.unit_before(current.offset)
        {
            // A statement left incomplete, as by a trailing operator, is
            // reported at the start of the next one. Discarding it must get
            // the parse past the reported error.
            let attempt = lines.blank_units(&text, &[line]);
            match parse(&attempt) {
                Ok(module) => return recovered(module, attempt, error),
                Err(next) if next.offset > current.offset => {
                    text = attempt;
                    current = next;
                    continue;
                }
                Err(_) => {}
            }
        }
        let attempt = if !unbalanced.is_empty() {
            // Discarding these settles nothing about the reported error.
            previous_offset = None;
            lines.blank_units(&text, &unbalanced)
        } else if let Some(line) = lines.unit_at(current.offset, widening) {
            lines.blank_units(&text, &[line])
        } else {
            // Widening ran out, as for an error reported at the end of the
            // source: discard the first top-level unit whose removal changes
            // the error.
            let mut changed = None;
            for line in lines.top_level_units() {
                let attempt = lines.blank_units(&text, &[line]);
                match parse(&attempt) {
                    Ok(module) => return recovered(module, attempt, error),
                    Err(next) if next != current => {
                        changed = Some((attempt, next));
                        break;
                    }
                    Err(_) => {}
                }
            }
            let Some((attempt, next)) = changed else {
                break;
            };
            text = attempt;
            current = next;
            previous_offset = None;
            continue;
        };
        match parse(&attempt) {
            Ok(module) => return recovered(module, attempt, error),
            Err(next) => current = next,
        }
        text = attempt;
    }
    RecoveredParse {
        module: None,
        source: text,
        error: Some(error),
    }
}

fn recovered(module: Module, source: String, error: ParseError) -> RecoveredParse {
    RecoveredParse {
        module: Some(module),
        source,
        error: Some(error),
    }
}

/// One source line, without its line break.
struct Line {
    start: usize,
    end: usize,
    /// Column of the line's first token, unless no token starts on it: a
    /// blank or comment-only line, or one inside a multi-line token.
    indent: Option<usize>,
    /// Offset of the line's first token.
    first: Option<usize>,
    closes: bool,
    doc_comment: bool,
    /// The bracket tokens starting on the line, in order.
    brackets: Vec<TokenKind>,
}

struct Lines {
    lines: Vec<Line>,
    /// Index past the last line of each line's unit.
    unit_ends: Vec<usize>,
    /// For a closing-bracket line inside another line's unit, that line. It
    /// closes that unit rather than starting one.
    closed_units: Vec<Option<usize>>,
}

impl Lines {
    fn new(source: &str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                let end = if index > start && source.as_bytes()[index - 1] == b'\r' {
                    index - 1
                } else {
                    index
                };
                lines.push(Line::empty(start, end));
                start = index + 1;
            }
        }
        lines.push(Line::empty(start, source.len()));

        let mut line = 0;
        for token in lex(source) {
            while lines[line].end < token.span.start && line + 1 < lines.len() {
                line += 1;
            }
            let entry = &mut lines[line];
            match token.kind {
                TokenKind::Whitespace | TokenKind::Newline | TokenKind::BlockComment => {}
                TokenKind::LineComment => {
                    if entry.indent.is_none() && token.text.starts_with("///") {
                        entry.doc_comment = true;
                    }
                }
                kind => {
                    if entry.indent.is_none() && !entry.doc_comment {
                        entry.indent = Some(source[entry.start..token.span.start].chars().count());
                        entry.first = Some(token.span.start);
                        entry.closes = matches!(
                            kind,
                            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket
                        );
                    }
                    if matches!(
                        kind,
                        TokenKind::LParen
                            | TokenKind::RParen
                            | TokenKind::LBrace
                            | TokenKind::RBrace
                            | TokenKind::LBracket
                            | TokenKind::RBracket
                    ) {
                        entry.brackets.push(kind);
                    }
                }
            }
        }

        let mut closed_units = vec![None; lines.len()];
        let unit_ends = (0..lines.len())
            .map(|index| {
                let Some(indent) = lines[index].indent else {
                    return index + 1;
                };
                let mut end = index + 1;
                for (offset, line) in lines[index + 1..].iter().enumerate() {
                    match line.indent {
                        None => {}
                        Some(next) if next > indent => end = index + offset + 2,
                        Some(next) if next == indent && line.closes => {
                            end = index + offset + 2;
                            closed_units[end - 1].get_or_insert(index);
                        }
                        Some(_) => break,
                    }
                }
                end
            })
            .collect();
        Self {
            lines,
            unit_ends,
            closed_units,
        }
    }

    fn starts_unit(&self, line: usize) -> bool {
        self.lines[line].indent.is_some() && self.closed_units[line].is_none()
    }

    fn balanced(&self, lines: std::ops::Range<usize>) -> bool {
        let mut open = Vec::new();
        for kind in self.lines[lines].iter().flat_map(|line| &line.brackets) {
            let closes = match kind {
                TokenKind::RParen => TokenKind::LParen,
                TokenKind::RBrace => TokenKind::LBrace,
                TokenKind::RBracket => TokenKind::LBracket,
                opening => {
                    open.push(*opening);
                    continue;
                }
            };
            if open.pop() != Some(closes) {
                return false;
            }
        }
        open.is_empty()
    }

    /// The first lines of the units with unbalanced brackets that hold no
    /// smaller unit with unbalanced brackets.
    fn innermost_unbalanced_units(&self) -> Vec<usize> {
        let unbalanced = (0..self.lines.len())
            .map(|line| self.starts_unit(line) && !self.balanced(line..self.unit_ends[line]))
            .collect::<Vec<_>>();
        (0..self.lines.len())
            .filter(|&line| {
                unbalanced[line] && !(line + 1..self.unit_ends[line]).any(|inner| unbalanced[inner])
            })
            .collect()
    }

    /// The first line of the unit holding `offset`, widened `widening` times
    /// to the enclosing unit.
    fn unit_at(&self, offset: usize, widening: usize) -> Option<usize> {
        let mut line = self
            .lines
            .iter()
            .rposition(|line| line.start <= offset)
            .unwrap_or(0);
        while self.lines[line].indent.is_none() {
            line = line.checked_sub(1)?;
        }
        if let Some(unit) = self.closed_units[line] {
            line = unit;
        }
        for _ in 0..widening {
            line = (0..line)
                .rev()
                .find(|&outer| self.starts_unit(outer) && self.unit_ends[outer] > line)?;
        }
        Some(line)
    }

    /// The first lines of the units no other unit holds.
    fn top_level_units(&self) -> impl Iterator<Item = usize> + '_ {
        let mut line = 0;
        std::iter::from_fn(move || {
            while line < self.lines.len() && !self.starts_unit(line) {
                line += 1;
            }
            let unit = (line < self.lines.len()).then_some(line)?;
            line = self.unit_ends[unit];
            Some(unit)
        })
    }

    /// The first line of the unit before the one whose first token is at
    /// `offset`, if a unit starts there.
    fn unit_before(&self, offset: usize) -> Option<usize> {
        let line = self
            .lines
            .iter()
            .position(|line| line.first == Some(offset))?;
        let start = self.lines[line].start;
        self.unit_at(start.checked_sub(1)?, 0)
            .filter(|&previous| previous < line)
    }

    /// Returns `text` with the units starting at `lines`, and the doc
    /// comments right before each, overwritten by spaces.
    fn blank_units(&self, text: &str, lines: &[usize]) -> String {
        let mut text = text.as_bytes().to_vec();
        for &line in lines {
            let mut first = line;
            while first > 0 && self.lines[first - 1].doc_comment {
                first -= 1;
            }
            let start = self.lines[first].start;
            let end = self.lines[self.unit_ends[line] - 1].end;
            for byte in &mut text[start..end] {
                if !matches!(*byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
        }
        String::from_utf8(text).expect("blanking keeps the source UTF-8")
    }
}

impl Line {
    fn empty(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            indent: None,
            first: None,
            closes: false,
            doc_comment: false,
            brackets: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of `source` recovery discards, as their original text.
    fn discarded(source: &str) -> Vec<&str> {
        let recovered = parse_recovering(source);
        assert!(recovered.module.is_some(), "{source:?} did not recover");
        assert!(recovered.error.is_some());
        assert_eq!(recovered.source.len(), source.len());
        source
            .lines()
            .zip(recovered.source.lines())
            .filter(|(original, recovered)| original != recovered)
            .map(|(original, recovered)| {
                assert!(
                    recovered.trim().is_empty(),
                    "{recovered:?} is partly blanked"
                );
                original
            })
            .collect()
    }

    #[test]
    fn leaves_source_that_parses_alone() {
        let source = "def double = (x: I32) => x * 2\nlet value = double 4\n";
        let recovered = parse_recovering(source);
        assert_eq!(recovered.module, parse(source).ok());
        assert_eq!(recovered.source, source);
        assert!(recovered.error.is_none());
    }

    #[test]
    fn discards_only_the_broken_statement_of_a_body() {
        assert_eq!(
            discarded(
                "def f = (a: I32) => {\n    let x = a\n    let y: = 1\n    x\n}\nlet z = 1\n"
            ),
            ["    let y: = 1"],
        );
    }

    #[test]
    fn discards_a_statement_with_an_unclosed_bracket() {
        assert_eq!(
            discarded("def f = (a: I32) => {\n    let x = a\n    f (x\n    x\n}\nlet z = 1\n"),
            ["    f (x"],
        );
    }

    #[test]
    fn discards_a_broken_item_between_items() {
        assert_eq!(
            discarded("let a = 1\n/// Broken.\nlet b: = 2\nlet c = 3\n"),
            ["/// Broken.", "let b: = 2"],
        );
    }

    #[test]
    fn finds_the_broken_unit_of_an_error_at_the_end() {
        assert_eq!(
            discarded("let a = 1\nlet flag:\n\nlet b = a\n"),
            ["let flag:"],
        );
    }

    #[test]
    fn keeps_offsets_through_multibyte_text() {
        assert_eq!(
            discarded("let café = 1\nlet é = \"ü\" +\nlet b = café\n"),
            ["let é = \"ü\" +"]
        );
    }
}
