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
    Package,
    As,
    Satisfies,
    Pub,
    Let,
    Mut,
    Move,
    Signal,
    Return,
    Loop,
    Break,
    Continue,
    Def,
    Const,
    Extern,
    Type,
    Mod,
    Companion,
    Macro,
    Trait,
    Impl,
    Match,
    Alias,
    Opaque,
    Where,
    Underscore,
    Identifier,
    String,
    Integer,
    Float,
    Whitespace,
    Newline,
    LineComment,
    BlockComment,
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
    Bang,
    Operator,
    Star,
    Plus,
    Minus,
    Slash,
    Unknown,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::Newline | Self::LineComment | Self::BlockComment
        )
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
    pub(crate) origin: Option<Span>,
}

impl SyntaxToken {
    pub fn origin(&self) -> Option<&Span> {
        self.origin.as_ref()
    }
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
    pub(crate) identifier_origins: Vec<(String, Span)>,
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

    /// True when this syntax was produced by macro expansion rather than parsed
    /// directly from user source. Tooling that reconstructs source (for example
    /// `staple expand`) uses this to decide whether a node's own token text
    /// already reflects the expanded result.
    pub fn is_generated(&self) -> bool {
        self.expansion_mark.is_some()
    }

    /// The half-open range this node occupies inside [`Self::token_stream`].
    /// Only meaningful relative to another node that [`Self::shares_token_stream`]
    /// with this one.
    pub fn token_range(&self) -> Range<usize> {
        self.token_range.clone()
    }

    /// The full backing token stream this node is a slice of. Nodes parsed
    /// together share one stream; nodes from different parses (such as a
    /// macro expansion spliced into user code) do not.
    pub fn token_stream(&self) -> &[SyntaxToken] {
        &self.tokens
    }

    /// Whether `self` and `other` are slices of the same backing token stream,
    /// so their [`Self::token_range`]s can be compared.
    pub fn shares_token_stream(&self, other: &Syntax) -> bool {
        Arc::ptr_eq(&self.tokens, &other.tokens)
    }

    pub fn compiler() -> Self {
        Self {
            id: SyntaxId::COMPILER,
            span: Span::Compiler,
            tokens: Arc::from([]),
            token_range: 0..0,
            definition_module: None,
            expansion_mark: None,
            identifier_origins: Vec::new(),
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
            identifier_origins: Vec::new(),
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

    pub(crate) fn record_identifier_origin(&mut self, name: String, syntax: &Syntax) {
        if let Some(origin) = syntax.identifier_origin(&name, false) {
            self.identifier_origins.push((name, origin.clone()));
            return;
        }
        let Some(token) = syntax
            .tokens()
            .iter()
            .find(|token| !token.kind.is_trivia() && token.text == name)
        else {
            return;
        };
        let Span::User { source, .. } = &syntax.span else {
            return;
        };
        self.identifier_origins.push((
            name,
            Span::User {
                source: source.clone(),
                range: token.span.clone(),
                location: None,
            },
        ));
    }

    pub fn identifier_origin(&self, name: &str, last: bool) -> Option<&Span> {
        let mut origins = self
            .identifier_origins
            .iter()
            .filter(|(origin_name, _)| origin_name == name);
        if last {
            origins.last().map(|(_, span)| span)
        } else {
            origins.next().map(|(_, span)| span)
        }
    }
}
