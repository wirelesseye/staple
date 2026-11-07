//! The macro-runtime intrinsic table.
//!
//! Each entry is one `std.syntax` function the host implements. Lowering
//! renames the declared binding to the entry's symbol, and both the test
//! harness and the future server read this table rather than repeating the
//! list.

use std::slice;

use staple_syntax::Syntax;

use super::arena::{self, Handle};
use crate::macro_expand::syntax_ops::{
    describe_position, syntax_slice, syntax_from_text as lex_syntax_from_text,
};

/// A host-provided syntax intrinsic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Intrinsic {
    /// The name macro code declares in `std.syntax`.
    pub(crate) name: &'static str,
    /// The external symbol lowering emits.
    #[allow(dead_code)] // Only the test harness links against the table so far.
    pub(crate) symbol: &'static str,
    /// Where the host implements the intrinsic. An entry with no host
    /// implementation points at a stub that stops the program with its name.
    #[allow(dead_code)] // Only the test harness links against the table so far.
    address: *const (),
}

impl Intrinsic {
    #[allow(dead_code)] // Only the test harness links against the table so far.
    pub(crate) fn address(self) -> usize {
        self.address as usize
    }
}

/// Builds one table entry, with a stand-in that names it.
macro_rules! intrinsic {
    ($name:literal, $symbol:literal) => {{
        extern "C" fn missing() -> usize {
            panic!(concat!("the macro runtime has no implementation of `", $name, "`"))
        }
        Intrinsic {
            name: $name,
            symbol: $symbol,
            address: missing as *const (),
        }
    }};
    ($name:literal, $symbol:literal, $function:path) => {
        Intrinsic {
            name: $name,
            symbol: $symbol,
            address: $function as *const (),
        }
    };
}

/// The closed list of host-provided syntax intrinsics.
pub(crate) const INTRINSICS: &[Intrinsic] = &[
    intrinsic!("stream", "__staple_syntax_stream", stream),
    // The interpreted `is_empty` and `error_at` have their own bodies; these
    // entries keep the old names resolvable and mark them unimplemented for
    // compiled code.
    intrinsic!("is_empty", "__staple_syntax_is_empty"),
    intrinsic!("error_at", "__staple_syntax_error_at"),
    intrinsic!("__is_empty", "__staple_syntax_is_empty", is_empty),
    intrinsic!("__error_at", "__staple_syntax_error_at", error_at),
    intrinsic!("remaining", "__staple_syntax_remaining", remaining),
    intrinsic!("compile_error", "__staple_syntax_compile_error"),
    intrinsic!("__parse_primitive", "__staple_syntax_parse_primitive"),
    intrinsic!("__parenthesized_group", "__staple_syntax_parenthesized_group"),
    intrinsic!("__bracketed_group", "__staple_syntax_bracketed_group"),
    intrinsic!("__angled_group", "__staple_syntax_angled_group"),
    intrinsic!("__braced_group", "__staple_syntax_braced_group"),
    intrinsic!("__parenthesized", "__staple_syntax_parenthesized"),
    intrinsic!("__bracketed", "__staple_syntax_bracketed"),
    intrinsic!("__angled", "__staple_syntax_angled"),
    intrinsic!("__braced", "__staple_syntax_braced"),
    intrinsic!("__take_rest", "__staple_syntax_take_rest", take_rest),
    intrinsic!("__offset", "__staple_syntax_offset", offset),
    intrinsic!("__sequence_push", "__staple_syntax_sequence_push"),
    intrinsic!("__quote", "__staple_syntax_quote"),
    intrinsic!("__splice_node", "__staple_syntax_splice_node"),
    intrinsic!("__splice_join", "__staple_syntax_splice_join"),
    intrinsic!("__clone_syntax", "__staple_syntax_clone_syntax"),
    intrinsic!("__origin", "__staple_syntax_origin"),
    intrinsic!("__with_origin", "__staple_syntax_with_origin"),
    intrinsic!("__text_new", "__staple_syntax_text_new", text_new),
    intrinsic!("__text_length", "__staple_syntax_text_length", text_length),
    intrinsic!("__text_copy", "__staple_syntax_text_copy", text_copy),
    intrinsic!("__syntax_text", "__staple_syntax_syntax_text", syntax_text),
    intrinsic!(
        "__syntax_from_text",
        "__staple_syntax_syntax_from_text",
        syntax_from_text
    ),
];

/// The symbol lowering emits for the intrinsic macro code declares as `name`.
pub(crate) fn symbol_name(name: &str) -> &'static str {
    match name {
        "stream" => "__staple_syntax_stream",
        "is_empty" => "__staple_syntax_is_empty",
        "error_at" => "__staple_syntax_error_at",
        "__is_empty" => "__staple_syntax_is_empty",
        "__error_at" => "__staple_syntax_error_at",
        "remaining" => "__staple_syntax_remaining",
        "compile_error" => "__staple_syntax_compile_error",
        "__parse_primitive" => "__staple_syntax_parse_primitive",
        "__parenthesized_group" => "__staple_syntax_parenthesized_group",
        "__bracketed_group" => "__staple_syntax_bracketed_group",
        "__angled_group" => "__staple_syntax_angled_group",
        "__braced_group" => "__staple_syntax_braced_group",
        "__parenthesized" => "__staple_syntax_parenthesized",
        "__bracketed" => "__staple_syntax_bracketed",
        "__angled" => "__staple_syntax_angled",
        "__braced" => "__staple_syntax_braced",
        "__take_rest" => "__staple_syntax_take_rest",
        "__offset" => "__staple_syntax_offset",
        "__sequence_push" => "__staple_syntax_sequence_push",
        "__quote" => "__staple_syntax_quote",
        "__splice_node" => "__staple_syntax_splice_node",
        "__splice_join" => "__staple_syntax_splice_join",
        "__clone_syntax" => "__staple_syntax_clone_syntax",
        "__origin" => "__staple_syntax_origin",
        "__with_origin" => "__staple_syntax_with_origin",
        "__text_new" => "__staple_syntax_text_new",
        "__text_length" => "__staple_syntax_text_length",
        "__text_copy" => "__staple_syntax_text_copy",
        "__syntax_text" => "__staple_syntax_syntax_text",
        "__syntax_from_text" => "__staple_syntax_syntax_from_text",
        _ => "",
    }
}

/// The entry for a declared intrinsic, if the host provides it.
pub(crate) fn lookup(name: &str) -> Option<&'static Intrinsic> {
    INTRINSICS.iter().find(|intrinsic| intrinsic.name == name)
}

/// The syntax fragment a handle names. A stale or wrong handle is a bug in the
/// calling code, so it stops the call rather than reading another value.
fn syntax_value(handle: u64) -> Syntax {
    arena::with_arena(|arena| arena.syntax(handle.into()).cloned())
        .unwrap_or_else(|| panic!("syntax handle {handle:#x} is not a live syntax fragment"))
}

fn stream_parts(handle: u64) -> (u64, usize) {
    arena::with_arena(|arena| arena.stream(handle.into()))
        .map(|(fragment, offset)| (fragment.into(), offset))
        .unwrap_or_else(|| panic!("stream handle {handle:#x} is not a live parse stream"))
}

#[cfg(test)]
fn text_bytes(handle: u64) -> Vec<u8> {
    arena::with_arena(|arena| arena.text(handle.into()).map(<[u8]>::to_vec))
        .unwrap_or_else(|| panic!("text handle {handle:#x} is not live text"))
}

/// A cursor over the tokens of one syntax fragment.
extern "C" fn stream(syntax: u64) -> u64 {
    arena::with_arena(|arena| {
        let fragment = Handle::from(syntax);
        assert!(
            arena.syntax(fragment).is_some(),
            "stream requires a syntax fragment"
        );
        arena.insert_stream(fragment, 0).into()
    })
}

/// `1` when only trivia remains, `0` otherwise. The Staple wrapper turns this
/// into a `Bool`, so the ABI does not carry a sum layout.
extern "C" fn is_empty(stream: u64) -> u8 {
    let (fragment, offset) = stream_parts(stream);
    let syntax = syntax_value(fragment);
    u8::from(
        syntax.tokens()[offset..]
            .iter()
            .all(|token| token.kind.is_trivia()),
    )
}

/// The unconsumed rest of a stream, losslessly.
extern "C" fn remaining(stream: u64) -> u64 {
    let (fragment, offset) = stream_parts(stream);
    let syntax = syntax_value(fragment);
    let rest = syntax_slice(&syntax, offset, syntax.tokens().len());
    arena::with_arena(|arena| arena.insert_syntax(rest).into())
}

/// A text handle holding `"<message> at <position>"`.
extern "C" fn error_at(stream: u64, message: *const u8, message_length: usize) -> u64 {
    let message = unsafe { slice::from_raw_parts(message, message_length) };
    let message = String::from_utf8_lossy(message);
    let (fragment, offset) = stream_parts(stream);
    let syntax = syntax_value(fragment);
    let text = format!("{message} at {}", describe_position(&syntax, offset));
    arena::with_arena(|arena| arena.insert_text(text.into_bytes()).into())
}

/// The cursor's position within its fragment.
extern "C" fn offset(stream: u64) -> usize {
    stream_parts(stream).1
}

#[repr(C)]
struct TakeRest {
    rest: u64,
    stream: u64,
}

/// Consumes the rest of a stream and returns it with the advanced cursor.
extern "C" fn take_rest(stream: u64) -> TakeRest {
    let (fragment, offset) = stream_parts(stream);
    let syntax = syntax_value(fragment);
    let end = syntax.tokens().len();
    let rest = syntax_slice(&syntax, offset, end);
    arena::with_arena(|arena| TakeRest {
        rest: arena.insert_syntax(rest).into(),
        stream: arena.insert_stream(fragment.into(), end).into(),
    })
}

/// Copies `length` bytes into the arena and returns their text handle.
extern "C" fn text_new(bytes: *const u8, length: usize) -> u64 {
    let bytes = unsafe { slice::from_raw_parts(bytes, length) };
    arena::with_arena(|arena| arena.insert_text(bytes.to_vec()).into())
}

/// The number of bytes in a text handle.
extern "C" fn text_length(text: u64) -> usize {
    arena::with_arena(|arena| arena.text(text.into()).map(<[u8]>::len))
        .unwrap_or_else(|| panic!("text handle {text:#x} is not live text"))
}

/// Copies a text handle's bytes into `destination`, returning the byte count.
extern "C" fn text_copy(text: u64, destination: *mut u8, capacity: usize) -> usize {
    arena::with_arena(|arena| {
        let bytes = arena
            .text(text.into())
            .unwrap_or_else(|| panic!("text handle {text:#x} is not live text"));
        let written = bytes.len().min(capacity);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, written);
        }
        written
    })
}

/// The lossless text of a syntax fragment, as a text handle.
extern "C" fn syntax_text(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    arena::with_arena(|arena| arena.insert_text(syntax.text().into_bytes()).into())
}

/// Lexes `length` bytes into a syntax fragment.
extern "C" fn syntax_from_text(bytes: *const u8, length: usize) -> u64 {
    let bytes = unsafe { slice::from_raw_parts(bytes, length) };
    let text = std::str::from_utf8(bytes).expect("syntax text is valid UTF-8");
    arena::with_arena(|arena| {
        let mut next = arena.next_syntax_id();
        let syntax = lex_syntax_from_text(text, &mut next);
        arena.insert_syntax(syntax).into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macro_runtime::arena::{reset, with_arena};

    fn with_syntax<T>(text: &str, f: impl FnOnce(u64) -> T) -> T {
        reset();
        let mut next = 0;
        let syntax = crate::macro_expand::syntax_ops::syntax_from_text(text, &mut next);
        let handle = with_arena(|arena| arena.insert_syntax(syntax).into());
        f(handle)
    }

    fn stream_text(handle: u64) -> String {
        let (fragment, offset) = stream_parts(handle);
        let syntax = syntax_value(fragment);
        syntax.tokens()[offset..]
            .iter()
            .map(|token| token.text.as_str())
            .collect()
    }

    #[test]
    fn a_stream_starts_at_the_beginning() {
        with_syntax("one two", |syntax| {
            let stream = stream(syntax);
            assert_eq!(offset(stream), 0);
            assert_eq!(is_empty(stream), 0);
            assert_eq!(stream_text(stream), "one two");
            assert_eq!(syntax_text_of(remaining(stream)), "one two");
        });
    }

    #[test]
    fn a_stream_is_empty_when_only_trivia_remains() {
        with_syntax("one  ", |syntax| {
            let stream = stream(syntax);
            let rest = take_rest(stream);
            assert_eq!(is_empty(rest.stream), 1);
            assert_eq!(offset(rest.stream), syntax_text_length(syntax));
            assert_eq!(syntax_text_of(rest.rest), "one  ");
        });
        with_syntax("   \n", |syntax| {
            assert_eq!(is_empty(stream(syntax)), 1);
        });
    }

    #[test]
    fn taking_the_rest_advances_a_new_stream() {
        with_syntax("one two", |syntax| {
            let start = stream(syntax);
            let taken = take_rest(start);
            assert_eq!(syntax_text_of(taken.rest), "one two");
            assert_eq!(offset(taken.stream), 3);
            assert_eq!(is_empty(taken.stream), 1);
            // The original stream is a fork and still starts at the beginning.
            assert_eq!(offset(start), 0);
            assert_eq!(stream_text(start), "one two");
        });
    }

    #[test]
    fn remaining_keeps_the_unread_tokens() {
        with_syntax("one two three", |syntax| {
            let start = stream(syntax);
            // Advance by taking everything, then fork from the original.
            let taken = take_rest(start);
            assert_eq!(syntax_text_of(taken.rest), "one two three");
            assert_eq!(syntax_text_of(remaining(start)), "one two three");
        });
    }

    #[test]
    fn an_error_names_the_current_position() {
        with_syntax("one\ntwo", |syntax| {
            let stream = stream(syntax);
            let message = b"expected an expression";
            let error = error_at(stream, message.as_ptr(), message.len());
            assert_eq!(
                String::from_utf8(text_bytes(error)).unwrap(),
                "expected an expression at line 1, column 1"
            );
        });
    }

    #[test]
    fn text_crosses_in_both_directions() {
        let bytes = b"one two";
        let handle = text_new(bytes.as_ptr(), bytes.len());
        assert_eq!(text_length(handle), 7);
        let mut destination = [0u8; 4];
        let written = text_copy(handle, destination.as_mut_ptr(), destination.len());
        assert_eq!(written, 4);
        assert_eq!(&destination, b"one ");

        with_syntax("one two", |syntax| {
            let text = syntax_text(syntax);
            assert_eq!(text_length(text), 7);
            let mut destination = [0u8; 7];
            assert_eq!(text_copy(text, destination.as_mut_ptr(), 7), 7);
            assert_eq!(&destination, b"one two");
        });
    }

    #[test]
    fn syntax_can_be_lexed_from_text() {
        let bytes = b"one two";
        let syntax = syntax_from_text(bytes.as_ptr(), bytes.len());
        assert_eq!(syntax_text_of(syntax), "one two");
        let round_trip = syntax_text(syntax);
        assert_eq!(text_bytes(round_trip), b"one two");
    }

    #[test]
    fn a_stale_handle_is_rejected() {
        reset();
        let mut next = 0;
        let syntax = crate::macro_expand::syntax_ops::syntax_from_text("one", &mut next);
        let handle: u64 = with_arena(|arena| arena.insert_syntax(syntax).into());
        reset();
        assert!(
            with_arena(|arena| arena.syntax(handle.into()).is_none()),
            "a handle from an earlier call is stale"
        );
    }

    fn syntax_text_of(handle: u64) -> String {
        syntax_value(handle).text()
    }

    fn syntax_text_length(handle: u64) -> usize {
        syntax_value(handle).tokens().len()
    }

    #[test]
    fn a_wrong_kind_of_handle_is_rejected() {
        reset();
        let text_handle: u64 =
            with_arena(|arena| arena.insert_text("not syntax".as_bytes().to_vec()).into());
        assert!(
            with_arena(|arena| arena.syntax(text_handle.into()).is_none()),
            "a text handle is not a syntax fragment"
        );
    }
}
