//! The macro-runtime intrinsic table.
//!
//! Each entry is one `std.syntax` function the host implements. Lowering
//! renames the declared binding to the entry's symbol, and both the test
//! harness and the future server read this table rather than repeating the
//! list.

/// A host-provided syntax intrinsic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Intrinsic {
    /// The name macro code declares in `std.syntax`.
    pub(crate) name: &'static str,
    /// The external symbol lowering emits.
    #[allow(dead_code)] // Only the test harness links against the table so far.
    pub(crate) symbol: &'static str,
    /// Stops the program with the intrinsic's name.
    missing: extern "C" fn() -> usize,
}

impl Intrinsic {
    /// Where the host implements the intrinsic. No entry has an implementation
    /// yet, so each one stops the program and names itself rather than
    /// returning a handle that looks like "none".
    #[allow(dead_code)] // Only the test harness links against the table so far.
    pub(crate) fn address(self) -> usize {
        self.missing as *const () as usize
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
            missing,
        }
    }};
}

/// The closed list of host-provided syntax intrinsics.
pub(crate) const INTRINSICS: &[Intrinsic] = &[
    intrinsic!("stream", "__staple_syntax_stream"),
    intrinsic!("is_empty", "__staple_syntax_is_empty"),
    intrinsic!("remaining", "__staple_syntax_remaining"),
    intrinsic!("error_at", "__staple_syntax_error_at"),
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
    intrinsic!("__take_rest", "__staple_syntax_take_rest"),
    intrinsic!("__offset", "__staple_syntax_offset"),
    intrinsic!("__sequence_push", "__staple_syntax_sequence_push"),
    intrinsic!("__quote", "__staple_syntax_quote"),
    intrinsic!("__splice_node", "__staple_syntax_splice_node"),
    intrinsic!("__splice_join", "__staple_syntax_splice_join"),
    intrinsic!("__clone_syntax", "__staple_syntax_clone_syntax"),
    intrinsic!("__origin", "__staple_syntax_origin"),
    intrinsic!("__with_origin", "__staple_syntax_with_origin"),
];

/// The symbol lowering emits for the intrinsic macro code declares as `name`.
pub(crate) fn symbol_name(name: &str) -> &'static str {
    match name {
        "stream" => "__staple_syntax_stream",
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
        "__clone_syntax" => "__staple_syntax_clone_syntax",
        "__origin" => "__staple_syntax_origin",
        "__with_origin" => "__staple_syntax_with_origin",
        _ => "",
    }
}

/// The entry for a declared intrinsic, if the host provides it.
pub(crate) fn lookup(name: &str) -> Option<&'static Intrinsic> {
    INTRINSICS.iter().find(|intrinsic| intrinsic.name == name)
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
    fn a_missing_entry_has_its_own_stand_in() {
        let addresses = INTRINSICS
            .iter()
            .map(|intrinsic| intrinsic.address())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(addresses.len(), INTRINSICS.len());
    }

    #[test]
    fn the_table_matches_the_closed_list() {
        for name in [
            "stream",
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
            "__clone_syntax",
            "__origin",
            "__with_origin",
        ] {
            assert!(lookup(name).is_some(), "{name}");
        }
    }
}
