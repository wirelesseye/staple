use std::ops::Range;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxId(pub usize);

impl SyntaxId {
    pub const COMPILER: Self = Self(usize::MAX);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Use,
    As,
    Satisfies,
    Pub,
    Let,
    Var,
    Mut,
    Return,
    Loop,
    Break,
    Continue,
    Def,
    Extern,
    Type,
    Mod,
    Macro,
    Trait,
    Impl,
    Match,
    Alias,
    Opaque,
    Infix,
    Infixl,
    Infixr,
    Underscore,
    Identifier,
    String,
    Integer,
    Float,
    Whitespace,
    Newline,
    LineComment,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    Comma,
    Semicolon,
    Dot,
    Equals,
    Arrow,
    FatArrow,
    Ellipsis,
    Dollar,
    At,
    Backtick,
    Operator,
    Star,
    Plus,
    Minus,
    Slash,
    Unknown,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, Self::Whitespace | Self::Newline | Self::LineComment)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    User {
        source: Option<Arc<str>>,
        range: Range<usize>,
        location: Option<SourceLocation>,
    },
    Compiler,
}

impl Span {
    pub fn to_range(&self) -> Range<usize> {
        match self {
            Span::User { range, .. } => range.clone(),
            Span::Compiler => unreachable!(),
        }
    }
}

impl From<Range<usize>> for Span {
    fn from(value: Range<usize>) -> Self {
        Self::User {
            source: None,
            range: value,
            location: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxToken {
    pub kind: TokenKind,
    pub text: String,
    pub span: Range<usize>,
}

/// The exact source covered by an AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Syntax {
    pub id: SyntaxId,
    pub span: Span,
    pub(crate) tokens: Arc<[SyntaxToken]>,
    pub(crate) token_range: Range<usize>,
    pub(crate) definition_module: Option<usize>,
    pub(crate) expansion_mark: Option<u64>,
}

impl Syntax {
    pub fn text(&self) -> String {
        self.tokens()
            .iter()
            .map(|token| token.text.as_str())
            .collect()
    }

    pub fn tokens(&self) -> &[SyntaxToken] {
        &self.tokens[self.token_range.clone()]
    }

    pub fn compiler() -> Self {
        Self {
            id: SyntaxId::COMPILER,
            span: Span::Compiler,
            tokens: Arc::from([]),
            token_range: 0..0,
            definition_module: None,
            expansion_mark: None,
        }
    }

    pub(crate) fn synthetic(id: SyntaxId, span: Span) -> Self {
        Self {
            id,
            span,
            tokens: Arc::from([]),
            token_range: 0..0,
            definition_module: None,
            expansion_mark: None,
        }
    }

    pub(crate) fn generated(mut self, definition_module: usize, expansion_mark: u64) -> Self {
        self.definition_module = Some(definition_module);
        self.expansion_mark = Some(expansion_mark);
        self
    }

    pub(crate) fn definition_module(&self) -> Option<usize> {
        self.definition_module
    }
}
