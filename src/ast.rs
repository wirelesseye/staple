use std::fmt;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Let,
    Def,
    Extern,
    Const,
    Underscore,
    Identifier,
    String,
    Integer,
    Whitespace,
    Newline,
    LineComment,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Equals,
    Arrow,
    Ellipsis,
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
pub struct SyntaxToken {
    pub kind: TokenKind,
    pub text: String,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    SourceFile,
    ExternBlock,
    LetBinding,
    DefBinding,
    FunctionValue,
    Parameter,
    Type,
    BlockExpression,
    ListExpression,
    CallExpression,
    BinaryExpression,
    NameExpression,
    StringExpression,
    IntegerExpression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxNode {
    pub kind: NodeKind,
    pub span: Range<usize>,
    pub tokens: Vec<SyntaxToken>,
    pub children: Vec<SyntaxNode>,
}

impl SyntaxNode {
    /// Reconstruct this node exactly, including comments, whitespace and newlines.
    pub fn text(&self) -> String {
        self.tokens
            .iter()
            .map(|token| token.text.as_str())
            .collect()
    }

    pub fn children_of_kind(&self, kind: NodeKind) -> impl Iterator<Item = &SyntaxNode> {
        self.children.iter().filter(move |node| node.kind == kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}
