//! Macro arguments as the parent matched them.
//!
//! The parent matches a call against the macro's signature, as it always has,
//! and hands compiled code the match as an [`ArgTree`]. A generated entry
//! function decodes each parameter from its node through `FromMatch`, in
//! `std.syntax`, so the parent's matching decides what every parameter holds
//! and compiled code only builds the values.

use staple_syntax::Syntax;

use super::arena::{Handle, with_arena};

/// A node of a matched macro call.
#[derive(Clone, Debug)]
pub(crate) enum ArgTree {
    /// One fragment, which `FromMatch` parses as the parameter's type.
    Node(Syntax),
    /// A sequence's elements, a product's fields, or the parameters of a call.
    List(Vec<ArgTree>),
    /// An option that matched nothing.
    Absent,
    /// A delimited fragment, with its source and its contents.
    Group(Syntax, Box<ArgTree>),
}

impl ArgTree {
    fn kind(&self) -> u8 {
        match self {
            Self::Node(_) => 0,
            Self::List(_) => 1,
            Self::Absent => 2,
            Self::Group(_, _) => 3,
        }
    }
}

/// Puts a call's parameters into the arena, as one list with a child per
/// parameter, and returns the list's handle for the entry function.
#[allow(dead_code)] // Only the test harness makes calls so far.
pub(crate) fn insert_arguments(parameters: &[ArgTree]) -> u64 {
    let children = parameters.iter().map(insert).collect();
    with_arena(|arena| arena.insert_match(1, 0, children)).into()
}

fn insert(tree: &ArgTree) -> u64 {
    let (syntax, children) = match tree {
        ArgTree::Node(syntax) => (Some(syntax), Vec::new()),
        ArgTree::List(children) => (None, children.iter().map(insert).collect()),
        ArgTree::Absent => (None, Vec::new()),
        ArgTree::Group(source, contents) => (Some(source), vec![insert(contents)]),
    };
    let syntax = syntax.map_or(0, |syntax| {
        u64::from(with_arena(|arena| arena.insert_syntax(syntax.clone())))
    });
    with_arena(|arena| arena.insert_match(tree.kind(), syntax, children)).into()
}

fn match_node<T>(node: u64, read: impl FnOnce(u8, u64, &[u64]) -> T) -> T {
    with_arena(|arena| {
        let (kind, syntax, children) = arena
            .match_node(Handle::from(node))
            .unwrap_or_else(|| panic!("handle {node:#x} is not a live match node"));
        read(kind, syntax, children)
    })
}

/// 0 for a fragment, 1 for a list, 2 for an absent option, 3 for a group.
pub(crate) extern "C" fn match_kind(node: u64) -> u8 {
    match_node(node, |kind, _, _| kind)
}

/// A fragment's syntax, or a group's source.
pub(crate) extern "C" fn match_syntax(node: u64) -> u64 {
    let syntax = match_node(node, |_, syntax, _| syntax);
    assert_ne!(syntax, 0, "match node {node:#x} has no syntax");
    syntax
}

/// How many children a node has.
pub(crate) extern "C" fn match_count(node: u64) -> usize {
    match_node(node, |_, _, children| children.len())
}

/// One child of a node.
pub(crate) extern "C" fn match_child(node: u64, index: usize) -> u64 {
    match_node(node, |_, _, children| {
        *children
            .get(index)
            .unwrap_or_else(|| panic!("match node {node:#x} has no child {index}"))
    })
}
