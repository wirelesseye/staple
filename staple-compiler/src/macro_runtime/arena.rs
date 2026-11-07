//! Per-call storage for syntax fragments and parsed nodes.
//!
//! Compiled macro code sees `Syntax`, `ParseStream` and parsed nodes as `U64`
//! handles. The host keeps the values here, in one arena per macro call. A
//! handle carries the arena generation, so a value left over from an earlier
//! call is rejected rather than read as a different fragment.
//!
//! Quotation templates are not per-call state and live outside this arena.

use std::cell::RefCell;

use staple_syntax::{Expression, Item, ModifierInvocation, Pattern, Syntax, Type, TypeBody};

/// A parsed AST node the runtime keeps alongside its source fragment.
#[derive(Clone)]
#[allow(dead_code)] // Parsed-node handles arrive with the node views.
pub(crate) enum Node {
    Expression(Expression),
    Type(Type),
    Pattern(Pattern),
    Item(Box<Item>),
    Modifier(ModifierInvocation),
    TypeBody(TypeBody),
}

/// One value a handle can name.
#[derive(Clone)]
enum Entry {
    Syntax(Syntax),
    Text(Vec<u8>),
    /// A cursor into a syntax fragment. A stream is immutable: copying its
    /// handle forks it, and advancing allocates a new stream.
    Stream { fragment: u64, offset: usize },
    Node(Node),
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
}

impl Arena {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Ends the current call and makes every outstanding handle stale.
    pub(crate) fn reset(&mut self) {
        self.entries.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn insert_syntax(&mut self, syntax: Syntax) -> Handle {
        self.insert(Entry::Syntax(syntax))
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

    #[allow(dead_code)] // Parsed-node handles arrive with the node views.
    pub(crate) fn insert_node(&mut self, node: Node) -> Handle {
        self.insert(Entry::Node(node))
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

    #[allow(dead_code)] // Parsed-node handles arrive with the node views.
    pub(crate) fn node(&self, handle: Handle) -> Option<&Node> {
        match self.entry(handle)? {
            Entry::Node(node) => Some(node),
            _ => None,
        }
    }

    /// The next syntax id for a fragment the host lexes itself.
    pub(crate) fn next_syntax_id(&mut self) -> usize {
        let id = self.next_syntax_id;
        self.next_syntax_id += 1;
        id
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
#[allow(dead_code)] // The call boundary arrives with the entry functions.
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
    fn parsed_nodes_round_trip() {
        let mut arena = Arena::new();
        let mut next = 0;
        let source = fragment("one", &mut next);
        let expression = staple_syntax::parse_expression_fragment(&source, &mut next).unwrap();
        let handle = arena.insert_node(Node::Expression(expression));
        assert!(matches!(arena.node(handle), Some(Node::Expression(_))));
        assert_eq!(arena.syntax(handle), None);
    }

    #[test]
    fn handle_zero_names_nothing() {
        let arena = Arena::new();
        assert_eq!(arena.syntax(Handle::NONE), None);
        assert_eq!(arena.text(Handle::NONE), None);
        assert_eq!(arena.stream(Handle::NONE), None);
        assert!(arena.node(Handle::NONE).is_none());
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
