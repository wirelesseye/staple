//! Per-call storage for syntax fragments and parsed nodes.
//!
//! Compiled macro code sees `Syntax`, `ParseStream` and parsed nodes as `U64`
//! handles. The host keeps the values here, in one arena per macro call. A
//! handle carries the arena generation, so a value left over from an earlier
//! call is rejected rather than read as a different fragment.
//!
//! Quotation templates are not per-call state and live outside this arena; a
//! quotation being filled names its template by id.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use staple_syntax::{
    Expression, Item, ModifierInvocation, Pattern, Span, Syntax, SyntaxToken, Type, TypeBody,
};

use crate::macro_expand::syntax_ops::number_stream;

/// A fragment parsed as one grammar category. The arena keeps it beside the
/// fragment's handle, so asking several questions of one fragment parses it once.
#[derive(Clone)]
pub(crate) enum Node {
    Expression(Expression),
    Type(Type),
    Pattern(Pattern),
    Item(Box<Item>),
    Modifier(ModifierInvocation),
    TypeBody(TypeBody),
}

/// The grammar category a fragment was parsed as. One fragment can be read as
/// more than one category, so the parse cache is keyed by both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NodeKind {
    Expression,
    Type,
    Pattern,
    Item,
    Modifier,
    TypeBody,
}

/// The outcome of one host parse, kept in the arena so the host can return it
/// as a single handle. Every field is a handle or a status, so the ABI needs no
/// multi-field product.
#[derive(Clone, Default)]
pub(crate) struct ParseOutcome {
    pub(crate) status: u64,
    pub(crate) tag: u64,
    /// The parsed syntax, or a group's source syntax.
    pub(crate) syntax: u64,
    /// A leaf's advanced stream, or a group's contents stream.
    pub(crate) stream: u64,
    /// A group's outer stream advanced past the group.
    pub(crate) next: u64,
    /// A text handle holding a failure message.
    pub(crate) error: u64,
}

/// One value a handle can name.
#[derive(Clone)]
enum Entry {
    Syntax(Syntax),
    Text(Vec<u8>),
    /// A cursor into a syntax fragment. A stream is immutable: copying its
    /// handle forks it, and advancing allocates a new stream.
    Stream { fragment: u64, offset: usize },
    Parse(ParseOutcome),
    /// A quotation being filled: its template id and the syntax handed to it
    /// so far, one per distinct splice name.
    Quote { template: u64, splices: Vec<u64> },
    /// A node of a matched macro call: its kind, its syntax handle (a
    /// fragment's syntax or a group's source, or none) and its children.
    Match {
        kind: u8,
        syntax: u64,
        children: Vec<u64>,
    },
}

/// A handle into the arena. Zero means "none"; every other value names an
/// entry and the arena generation that created it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Handle(u64);

impl Handle {
    #[allow(dead_code)] // Callers name "none" with a plain zero.
    pub(crate) const NONE: Self = Self(0);

    fn new(index: usize, generation: u32) -> Self {
        debug_assert!(index < u32::MAX as usize);
        Self(((generation as u64) << 32) | (index as u64 + 1))
    }

    fn index(self) -> Option<usize> {
        let index = (self.0 & 0xffff_ffff) as usize;
        (index != 0).then(|| index - 1)
    }

    fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

impl From<Handle> for u64 {
    fn from(handle: Handle) -> Self {
        handle.0
    }
}

impl From<u64> for Handle {
    fn from(handle: u64) -> Self {
        Self(handle)
    }
}

/// Per-call storage. [`Self::reset`] ends a call: it drops every value and
/// starts a new generation, so old handles can no longer be read.
#[derive(Default)]
pub(crate) struct Arena {
    entries: Vec<Entry>,
    generation: u32,
    next_syntax_id: usize,
    /// Parses of syntax handles; `None` records that the fragment is not of
    /// that category.
    parsed: HashMap<(u64, NodeKind), Option<Node>>,
    /// The child fragments a part accessor handed out for a handle, so a
    /// rebuild compares against them without parsing the origin again.
    parts: HashMap<u64, Vec<Syntax>>,
    /// Streams whose spans [`own_offsets`] renumbered, by the stream they
    /// replace, so the nodes of one parse share one copy.
    renumbered: Vec<(Weak<[SyntaxToken]>, Arc<[SyntaxToken]>)>,
}

impl Arena {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Ends the current call and makes every outstanding handle stale.
    pub(crate) fn reset(&mut self) {
        self.entries.clear();
        self.parsed.clear();
        self.parts.clear();
        self.renumbered.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn insert_syntax(&mut self, mut syntax: Syntax) -> Handle {
        self.own_offsets(&mut syntax);
        self.insert(Entry::Syntax(syntax))
    }

    /// Makes `syntax`'s spans index its own stream's text.
    ///
    /// The parser builds the nodes of a fragment parse over a copy of just the
    /// fragment's tokens, whose spans still index the whole file. Parsing such
    /// a node again reads the text between its tokens from the copy, which
    /// doesn't have it. Compiled code does parse nodes again, so a stream that
    /// doesn't start at offset 0 gets fresh spans here, and each token keeps
    /// where it came from as its origin, as joining fragments does.
    fn own_offsets(&mut self, syntax: &mut Syntax) {
        if syntax.token_stream().first().is_none_or(|token| token.span.start == 0) {
            return;
        }
        let stream = Arc::downgrade(&syntax.tokens);
        self.renumbered.retain(|(replaced, _)| replaced.strong_count() > 0);
        if let Some((_, renumbered)) = self
            .renumbered
            .iter()
            .find(|(replaced, _)| Weak::ptr_eq(replaced, &stream))
        {
            syntax.tokens = Arc::clone(renumbered);
            return;
        }
        let source = match &syntax.span {
            Span::User { source, .. } => source.clone(),
            Span::Compiler => None,
        };
        let mut tokens = syntax.token_stream().to_vec();
        for token in &mut tokens {
            if token.origin.is_none() && !token.kind.is_trivia() {
                token.origin = Some(Span::User {
                    source: source.clone(),
                    range: token.span.clone(),
                    location: None,
                });
            }
        }
        number_stream(&mut tokens);
        let renumbered: Arc<[SyntaxToken]> = Arc::from(tokens);
        self.renumbered.push((stream, Arc::clone(&renumbered)));
        syntax.tokens = renumbered;
    }

    pub(crate) fn insert_text(&mut self, text: impl Into<Vec<u8>>) -> Handle {
        self.insert(Entry::Text(text.into()))
    }

    pub(crate) fn insert_stream(&mut self, fragment: Handle, offset: usize) -> Handle {
        self.insert(Entry::Stream {
            fragment: fragment.0,
            offset,
        })
    }

    pub(crate) fn insert_parse(&mut self, outcome: ParseOutcome) -> Handle {
        self.insert(Entry::Parse(outcome))
    }

    pub(crate) fn parse_outcome(&self, handle: Handle) -> Option<&ParseOutcome> {
        match self.entry(handle)? {
            Entry::Parse(outcome) => Some(outcome),
            _ => None,
        }
    }

    pub(crate) fn insert_quote(&mut self, template: u64) -> Handle {
        self.insert(Entry::Quote {
            template,
            splices: Vec::new(),
        })
    }

    /// Adds the next splice to a quotation being filled. Returns `false` when
    /// `handle` is not one.
    pub(crate) fn push_quote_splice(&mut self, handle: Handle, splice: u64) -> bool {
        let Some(index) = handle.index() else {
            return false;
        };
        if handle.generation() != self.generation {
            return false;
        }
        match self.entries.get_mut(index) {
            Some(Entry::Quote { splices, .. }) => {
                splices.push(splice);
                true
            }
            _ => false,
        }
    }

    /// A quotation's template id and splices.
    pub(crate) fn quote(&self, handle: Handle) -> Option<(u64, &[u64])> {
        match self.entry(handle)? {
            Entry::Quote { template, splices } => Some((*template, splices)),
            _ => None,
        }
    }

    pub(crate) fn insert_match(&mut self, kind: u8, syntax: u64, children: Vec<u64>) -> Handle {
        self.insert(Entry::Match {
            kind,
            syntax,
            children,
        })
    }

    /// A match node's kind, syntax handle and children.
    pub(crate) fn match_node(&self, handle: Handle) -> Option<(u8, u64, &[u64])> {
        match self.entry(handle)? {
            Entry::Match {
                kind,
                syntax,
                children,
            } => Some((*kind, *syntax, children)),
            _ => None,
        }
    }

    fn insert(&mut self, entry: Entry) -> Handle {
        let index = self.entries.len();
        self.entries.push(entry);
        Handle::new(index, self.generation)
    }

    fn entry(&self, handle: Handle) -> Option<&Entry> {
        let index = handle.index()?;
        (handle.generation() == self.generation)
            .then(|| self.entries.get(index))
            .flatten()
    }

    pub(crate) fn syntax(&self, handle: Handle) -> Option<&Syntax> {
        match self.entry(handle)? {
            Entry::Syntax(syntax) => Some(syntax),
            _ => None,
        }
    }

    pub(crate) fn text(&self, handle: Handle) -> Option<&[u8]> {
        match self.entry(handle)? {
            Entry::Text(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn stream(&self, handle: Handle) -> Option<(Handle, usize)> {
        match self.entry(handle)? {
            Entry::Stream { fragment, offset } => Some((Handle(*fragment), *offset)),
            _ => None,
        }
    }

    /// The cached parse of a syntax handle as `kind`: `None` when it has not
    /// been parsed, `Some(None)` when it is not of that category.
    pub(crate) fn parsed(&self, handle: Handle, kind: NodeKind) -> Option<Option<&Node>> {
        self.parsed
            .get(&(handle.0, kind))
            .map(Option::as_ref)
    }

    pub(crate) fn set_parsed(&mut self, handle: Handle, kind: NodeKind, node: Option<Node>) {
        self.parsed.insert((handle.0, kind), node);
    }

    /// Removes and returns a cached parse, so its children can be moved out
    /// rather than copied.
    pub(crate) fn take_parsed(&mut self, handle: Handle, kind: NodeKind) -> Option<Node> {
        self.parsed.remove(&(handle.0, kind)).flatten()
    }

    pub(crate) fn set_parts(&mut self, handle: Handle, parts: Vec<Syntax>) {
        self.parts.insert(handle.0, parts);
    }

    pub(crate) fn parts(&self, handle: Handle) -> Option<&[Syntax]> {
        self.parts.get(&handle.0).map(Vec::as_slice)
    }

    /// The next syntax id for a fragment the host lexes itself.
    pub(crate) fn next_syntax_id(&mut self) -> usize {
        let id = self.next_syntax_id;
        self.next_syntax_id += 1;
        id
    }

    /// Returns the next syntax id after a parser consumed a range of them.
    pub(crate) fn set_next_syntax_id(&mut self, next: usize) {
        self.next_syntax_id = next;
    }
}

thread_local! {
    static ARENA: RefCell<Arena> = RefCell::new(Arena::new());
}

/// Runs `f` against the current thread's arena.
pub(crate) fn with_arena<T>(f: impl FnOnce(&mut Arena) -> T) -> T {
    ARENA.with(|arena| f(&mut arena.borrow_mut()))
}

/// Ends the current call on this thread.
#[allow(dead_code)] // Only the test harness makes calls so far.
pub(crate) fn reset() {
    with_arena(Arena::reset);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macro_expand::syntax_ops::syntax_from_text;

    fn fragment(text: &str, next: &mut usize) -> Syntax {
        syntax_from_text(text, next)
    }

    #[test]
    fn syntax_handles_round_trip() {
        let mut arena = Arena::new();
        let mut next = 0;
        let handle = arena.insert_syntax(fragment("one two", &mut next));
        assert_eq!(arena.syntax(handle).map(Syntax::text), Some("one two".to_owned()));
        assert_eq!(arena.text(handle), None);
        assert_eq!(arena.stream(handle), None);
    }

    #[test]
    fn text_handles_round_trip() {
        let mut arena = Arena::new();
        let handle = arena.insert_text("hello".as_bytes().to_vec());
        assert_eq!(arena.text(handle), Some("hello".as_bytes()));
        assert_eq!(arena.syntax(handle), None);
    }

    #[test]
    fn reset_invalidates_every_handle() {
        let mut arena = Arena::new();
        let mut next = 0;
        let syntax = arena.insert_syntax(fragment("one", &mut next));
        let text = arena.insert_text("two".as_bytes().to_vec());
        let stream = arena.insert_stream(syntax, 0);
        arena.reset();
        assert_eq!(arena.syntax(syntax), None);
        assert_eq!(arena.text(text), None);
        assert_eq!(arena.stream(stream), None);
        let replacement = arena.insert_syntax(fragment("three", &mut next));
        assert_ne!(replacement, syntax, "a new call gets new handles");
        assert_eq!(
            arena.syntax(replacement).map(Syntax::text),
            Some("three".to_owned())
        );
    }

    #[test]
    fn a_stream_is_a_fragment_and_an_offset() {
        let mut arena = Arena::new();
        let mut next = 0;
        let fragment = arena.insert_syntax(fragment("one two", &mut next));
        let start = arena.insert_stream(fragment, 0);
        let middle = arena.insert_stream(fragment, 4);
        assert_eq!(arena.stream(start), Some((fragment, 0)));
        assert_eq!(arena.stream(middle), Some((fragment, 4)));
        assert_ne!(start, middle);
    }

    #[test]
    fn parses_are_cached_per_kind_and_cleared_by_reset() {
        let mut arena = Arena::new();
        let mut next = 0;
        let source = fragment("one", &mut next);
        let expression = staple_syntax::parse_expression_fragment(&source, &mut next).unwrap();
        let handle = arena.insert_syntax(source.clone());
        assert!(arena.parsed(handle, NodeKind::Expression).is_none());
        arena.set_parsed(handle, NodeKind::Expression, Some(Node::Expression(expression)));
        arena.set_parsed(handle, NodeKind::Item, None);
        assert!(matches!(
            arena.parsed(handle, NodeKind::Expression),
            Some(Some(Node::Expression(_)))
        ));
        assert!(matches!(arena.parsed(handle, NodeKind::Item), Some(None)));
        arena.set_parts(handle, vec![source]);
        assert_eq!(arena.parts(handle).map(<[Syntax]>::len), Some(1));
        assert!(arena.take_parsed(handle, NodeKind::Expression).is_some());
        assert!(arena.parsed(handle, NodeKind::Expression).is_none());
        arena.set_parsed(handle, NodeKind::Type, None);
        arena.reset();
        assert!(arena.parsed(handle, NodeKind::Type).is_none());
        assert!(arena.parts(handle).is_none());
    }

    #[test]
    fn handle_zero_names_nothing() {
        let arena = Arena::new();
        assert_eq!(arena.syntax(Handle::NONE), None);
        assert_eq!(arena.text(Handle::NONE), None);
        assert_eq!(arena.stream(Handle::NONE), None);
    }

    #[test]
    fn the_thread_arena_resets() {
        let mut next = 0;
        let handle = with_arena(|arena| {
            arena.reset();
            arena.insert_syntax(fragment("one", &mut next))
        });
        assert!(with_arena(|arena| arena.syntax(handle).is_some()));
        reset();
        assert!(with_arena(|arena| arena.syntax(handle).is_none()));
    }
}
