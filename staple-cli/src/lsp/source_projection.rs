use std::ops::Range;
use std::path::Path;

use staple_syntax::{Span, Syntax, SyntaxToken};

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
    let mut matching_tokens = syntax.tokens().iter().filter(|token| token.text == name);
    let token = if last {
        matching_tokens.next_back()
    } else {
        matching_tokens.next()
    };
    if let Some(origin) = token.and_then(SyntaxToken::origin)
        && belongs_to(origin, path)
        && let Span::User { range, .. } = origin
    {
        return Some(range.clone());
    }
    if let Some(origin) = syntax.identifier_origin(name, last)
        && belongs_to(origin, path)
        && let Span::User { range, .. } = origin
    {
        return Some(range.clone());
    }
    if !belongs_to(&syntax.span, path) {
        return None;
    }
    token.map(|token| token.span.clone())
}

pub fn syntax_range(syntax: &Syntax, path: &Path) -> Option<Range<usize>> {
    let significant = syntax
        .tokens()
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    if !significant.is_empty() {
        let origins = significant
            .iter()
            .map(|token| token.origin())
            .collect::<Option<Vec<_>>>();
        if let Some(origins) = origins
            && origins.iter().all(|origin| belongs_to(origin, path))
            && let (Span::User { range: first, .. }, Span::User { range: last, .. }) =
                (origins.first()?, origins.last()?)
        {
            return Some(first.start..last.end);
        }
    }
    if !belongs_to(&syntax.span, path) {
        return None;
    }
    let first = significant.first();
    let last = significant.last();
    match (first, last) {
        (Some(first), Some(last)) => Some(first.span.start..last.span.end),
        _ => match &syntax.span {
            Span::User { range, .. } => Some(range.clone()),
            Span::Compiler => None,
        },
    }
}
