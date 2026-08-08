use std::fmt;

use crate::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.span {
            Span::User { source, range } => {
                if let Some(source) = source {
                    write!(
                        formatter,
                        "{}: {} at byte {}",
                        source, self.message, range.start
                    )
                } else {
                    write!(formatter, "{} at byte {}", self.message, range.start)
                }
            }
            Span::Compiler => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for Diagnostic {}
