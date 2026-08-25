use std::ops::Range;
use std::path::Path;

use stapler::{Span, Syntax};

fn belongs_to(span: &Span, path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return !matches!(span, Span::Compiler);
    }
    match span {
        Span::User {
            source: Some(source),
            ..
        } => Path::new(source.as_ref()) == path,
        Span::User { source: None, .. } => true,
        Span::Compiler => false,
    }
}

pub fn named_range(syntax: &Syntax, name: &str, last: bool, path: &Path) -> Option<Range<usize>> {
    if let Some(origin) = syntax.identifier_origin(name, last)
        && belongs_to(origin, path)
        && let Span::User { range, .. } = origin
    {
        return Some(range.clone());
    }
    if !belongs_to(&syntax.span, path) {
        return None;
    }
    let mut tokens = syntax.tokens().iter().filter(|token| token.text == name);
    if last {
        tokens.next_back().map(|token| token.span.clone())
    } else {
        tokens.next().map(|token| token.span.clone())
    }
}

pub fn syntax_range(syntax: &Syntax, path: &Path) -> Option<Range<usize>> {
    if !belongs_to(&syntax.span, path) {
        return None;
    }
    let first = syntax.tokens().iter().find(|token| !token.kind.is_trivia());
    let last = syntax
        .tokens()
        .iter()
        .rev()
        .find(|token| !token.kind.is_trivia());
    match (first, last) {
        (Some(first), Some(last)) => Some(first.span.start..last.span.end),
        _ => match &syntax.span {
            Span::User { range, .. } => Some(range.clone()),
            Span::Compiler => None,
        },
    }
}
