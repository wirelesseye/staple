//! The macro-runtime intrinsic table.
//!
//! Each entry is one `std.syntax` function the host implements. Lowering
//! renames the declared binding to the entry's symbol, and both the test
//! harness and the future server read this table rather than repeating the
//! list.

/// A host-provided syntax intrinsic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Intrinsic {
    /// The name macro code declares in `std.syntax`.
    pub(crate) name: &'static str,
    /// The external symbol lowering emits.
    pub(crate) symbol: &'static str,
}

impl Intrinsic {
    /// Where the host implements the intrinsic. Until an implementation is
    /// added, all entries point at the same null-returning stub.
    pub(crate) fn address(self) -> usize {
        stub as *const () as usize
    }
}

/// The closed list of host-provided syntax intrinsics.
pub(crate) const INTRINSICS: &[Intrinsic] = &[
    intrinsic("stream", "__staple_syntax_stream"),
    intrinsic("duplicate_modifier", "__staple_syntax_duplicate_modifier"),
    intrinsic("is_empty", "__staple_syntax_is_empty"),
    intrinsic("remaining", "__staple_syntax_remaining"),
    intrinsic("error_at", "__staple_syntax_error_at"),
    intrinsic("compile_error", "__staple_syntax_compile_error"),
    intrinsic("__parse_primitive", "__staple_syntax_parse_primitive"),
    intrinsic("__parenthesized_group", "__staple_syntax_parenthesized_group"),
    intrinsic("__bracketed_group", "__staple_syntax_bracketed_group"),
    intrinsic("__angled_group", "__staple_syntax_angled_group"),
    intrinsic("__braced_group", "__staple_syntax_braced_group"),
    intrinsic("__parenthesized", "__staple_syntax_parenthesized"),
    intrinsic("__bracketed", "__staple_syntax_bracketed"),
    intrinsic("__angled", "__staple_syntax_angled"),
    intrinsic("__braced", "__staple_syntax_braced"),
    intrinsic("__take_rest", "__staple_syntax_take_rest"),
    intrinsic("__offset", "__staple_syntax_offset"),
    intrinsic("__sequence_push", "__staple_syntax_sequence_push"),
    intrinsic("__quote", "__staple_syntax_quote"),
    intrinsic("__splice_node", "__staple_syntax_splice_node"),
    intrinsic("__splice_join", "__staple_syntax_splice_join"),
    intrinsic("__origin", "__staple_syntax_origin"),
    intrinsic("__with_origin", "__staple_syntax_with_origin"),
];

const fn intrinsic(name: &'static str, symbol: &'static str) -> Intrinsic {
    Intrinsic { name, symbol }
}
/// The symbol lowering emits for the intrinsic macro code declares as `name`.
pub(crate) fn symbol_name(name: &str) -> &'static str {
    match name {
        "stream" => "__staple_syntax_stream",
        "duplicate_modifier" => "__staple_syntax_duplicate_modifier",
        "is_empty" => "__staple_syntax_is_empty",
        "remaining" => "__staple_syntax_remaining",
        "error_at" => "__staple_syntax_error_at",
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
        "__origin" => "__staple_syntax_origin",
        "__with_origin" => "__staple_syntax_with_origin",
        _ => "",
    }
}

/// The entry for a declared intrinsic, if the host provides it.
pub(crate) fn lookup(name: &str) -> Option<&'static Intrinsic> {
    INTRINSICS.iter().find(|intrinsic| intrinsic.name == name)
}

/// Stands in for an intrinsic that has no implementation yet. It returns a
/// null handle, which every consumer treats as "no value".
pub(crate) extern "C" fn stub() -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_has_the_prefix_and_no_duplicates() {
        let mut names = std::collections::HashSet::new();
        for intrinsic in INTRINSICS {
            assert!(intrinsic.symbol.starts_with("__staple_syntax_"));
            assert!(names.insert(intrinsic.name), "duplicate {}", intrinsic.name);
        }
    }

    #[test]
    fn the_table_matches_the_closed_list() {
        for name in [
            "stream",
            "duplicate_modifier",
            "is_empty",
            "remaining",
            "error_at",
            "compile_error",
            "__parse_primitive",
            "__parenthesized_group",
            "__bracketed_group",
            "__angled_group",
            "__braced_group",
            "__parenthesized",
            "__bracketed",
            "__angled",
            "__braced",
            "__take_rest",
            "__offset",
            "__sequence_push",
            "__quote",
            "__splice_node",
            "__splice_join",
            "__origin",
            "__with_origin",
        ] {
            assert!(lookup(name).is_some(), "{name}");
        }
    }
}
