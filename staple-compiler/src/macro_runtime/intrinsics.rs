//! The macro-runtime intrinsic table.
//!
//! Each entry is one `std.syntax` function the host implements. Lowering
//! renames the declared binding to the entry's symbol, and both the test
//! harness and the future server read this table rather than repeating the
//! list.

use std::slice;
use std::sync::Arc;

use staple_syntax::{
    Expression, Item, Span, Syntax, SyntaxId, SyntaxToken, TokenKind, string_literal,
};

use super::arena::{self, Handle};
use crate::macro_expand::syntax_ops::{
    append_syntax_tokens, describe_position, is_structured_product_type,
    syntax_from_text as lex_syntax_from_text, syntax_slice,
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
    intrinsic!("__sequence_push", "__staple_syntax_sequence_push"),
    intrinsic!("__sequence_reverse", "__staple_syntax_sequence_reverse"),
    intrinsic!("__offset", "__staple_syntax_offset", offset),
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
    intrinsic!("__classify_expr", "__staple_syntax_classify_expr", classify_expr),
    intrinsic!("__call_parts", "__staple_syntax_call_parts", call_parts),
    intrinsic!("__ident_text", "__staple_syntax_ident_text", ident_text),
    intrinsic!(
        "__string_expr_text",
        "__staple_syntax_string_expr_text",
        string_expr_text
    ),
    intrinsic!(
        "__char_expr_char",
        "__staple_syntax_char_expr_char",
        char_expr_char
    ),
    intrinsic!(
        "__unstructured_expr",
        "__staple_syntax_unstructured_expr",
        unstructured_expr
    ),
    intrinsic!(
        "__unstructured_expr_syntax",
        "__staple_syntax_unstructured_expr_syntax",
        unstructured_expr
    ),
    intrinsic!(
        "__unstructured_type_syntax",
        "__staple_syntax_unstructured_type_syntax",
        unstructured_type
    ),
    intrinsic!(
        "__unstructured_pattern_syntax",
        "__staple_syntax_unstructured_pattern_syntax",
        unstructured_pattern
    ),
    intrinsic!(
        "__unstructured_item_syntax",
        "__staple_syntax_unstructured_item_syntax",
        unstructured_item
    ),
    intrinsic!(
        "__unstructured_modifier_syntax",
        "__staple_syntax_unstructured_modifier_syntax",
        unstructured_modifier
    ),
    intrinsic!("__call_syntax", "__staple_syntax_call_syntax", call_syntax),
    intrinsic!("__ident_syntax", "__staple_syntax_ident_syntax", ident_syntax),
    intrinsic!("__ident_spelling", "__staple_syntax_ident_spelling"),
    intrinsic!(
        "__string_expr_syntax",
        "__staple_syntax_string_expr_syntax",
        string_expr_syntax
    ),
    intrinsic!(
        "__char_expr_syntax",
        "__staple_syntax_char_expr_syntax",
        char_expr_syntax
    ),
    intrinsic!("__classify_type", "__staple_syntax_classify_type", classify_type),
    intrinsic!(
        "__product_type_field_count",
        "__staple_syntax_product_type_field_count",
        product_type_field_count
    ),
    intrinsic!(
        "__product_type_field",
        "__staple_syntax_product_type_field",
        product_type_field
    ),
    intrinsic!(
        "__product_type_field_parts",
        "__staple_syntax_product_type_field_parts",
        product_type_field_parts
    ),
    intrinsic!(
        "__product_type_field_name",
        "__staple_syntax_product_type_field_name",
        product_type_field_name
    ),
    intrinsic!(
        "__product_type_field_type",
        "__staple_syntax_product_type_field_type",
        product_type_field_type
    ),
    intrinsic!(
        "__product_type_field_default",
        "__staple_syntax_product_type_field_default",
        product_type_field_default
    ),
    intrinsic!(
        "__product_type_syntax",
        "__staple_syntax_product_type_syntax",
        product_type_syntax
    ),
    intrinsic!(
        "__join_field_syntax",
        "__staple_syntax_join_field_syntax",
        join_field_syntax
    ),
    intrinsic!(
        "__product_type_field_syntax",
        "__staple_syntax_product_type_field_syntax",
        product_type_field_syntax
    ),
    intrinsic!(
        "__unstructured_type",
        "__staple_syntax_unstructured_type",
        unstructured_type_from_syntax
    ),
    intrinsic!(
        "__unstructured_pattern",
        "__staple_syntax_unstructured_pattern",
        unstructured_pattern_from_syntax
    ),
    intrinsic!(
        "__unstructured_item",
        "__staple_syntax_unstructured_item",
        unstructured_item_from_syntax
    ),
    intrinsic!(
        "__unstructured_modifier",
        "__staple_syntax_unstructured_modifier",
        unstructured_modifier_from_syntax
    ),
    intrinsic!("__empty_syntax", "__staple_syntax_empty_syntax", empty_syntax),
    intrinsic!(
        "__syntax_is_empty",
        "__staple_syntax_syntax_is_empty",
        syntax_is_empty
    ),
    intrinsic!(
        "__same_syntax",
        "__staple_syntax_same_syntax",
        same_syntax
    ),
    intrinsic!(
        "__syntax_or_origin",
        "__staple_syntax_syntax_or_origin",
        syntax_or_origin
    ),
    intrinsic!(
        "__join_separated_syntax",
        "__staple_syntax_join_separated_syntax",
        join_separated_syntax
    ),
    intrinsic!(
        "__append_separator_syntax",
        "__staple_syntax_append_separator_syntax",
        append_separator_syntax
    ),
    intrinsic!(
        "__classify_pattern",
        "__staple_syntax_classify_pattern",
        classify_pattern
    ),
    intrinsic!(
        "__binding_pattern_name",
        "__staple_syntax_binding_pattern_name",
        binding_pattern_name
    ),
    intrinsic!(
        "__nominal_pattern_parts",
        "__staple_syntax_nominal_pattern_parts",
        nominal_pattern_parts
    ),
    intrinsic!(
        "__binding_pattern_syntax",
        "__staple_syntax_binding_pattern_syntax",
        binding_pattern_syntax
    ),
    intrinsic!(
        "__nominal_pattern_syntax",
        "__staple_syntax_nominal_pattern_syntax",
        nominal_pattern_syntax
    ),
    intrinsic!(
        "__sequence_syntax",
        "__staple_syntax_sequence_syntax",
        sequence_syntax
    ),
    intrinsic!("__classify_item", "__staple_syntax_classify_item", classify_item),
    intrinsic!(
        "__modified_item_parts",
        "__staple_syntax_modified_item_parts",
        modified_item_parts
    ),
    intrinsic!(
        "__modified_item_modifier",
        "__staple_syntax_modified_item_modifier",
        modified_item_modifier
    ),
    intrinsic!(
        "__type_declaration_parts",
        "__staple_syntax_type_declaration_parts",
        type_declaration_parts
    ),
    intrinsic!(
        "__type_declaration_name",
        "__staple_syntax_type_declaration_name",
        type_declaration_name
    ),
    intrinsic!(
        "__type_declaration_declared_type",
        "__staple_syntax_type_declaration_declared_type",
        type_declaration_declared_type
    ),
    intrinsic!(
        "__type_declaration_type_parameter_count",
        "__staple_syntax_type_declaration_type_parameter_count",
        type_declaration_type_parameter_count
    ),
    intrinsic!(
        "__type_declaration_body",
        "__staple_syntax_type_declaration_body",
        type_declaration_body
    ),
    intrinsic!(
        "__type_declaration_resource_count",
        "__staple_syntax_type_declaration_resource_count",
        type_declaration_resource_count
    ),
    intrinsic!(
        "__type_declaration_type_parameter",
        "__staple_syntax_type_declaration_type_parameter",
        type_declaration_type_parameter
    ),
    intrinsic!(
        "__type_declaration_resource",
        "__staple_syntax_type_declaration_resource",
        type_declaration_resource
    ),
    intrinsic!(
        "__classify_modifier",
        "__staple_syntax_classify_modifier",
        classify_modifier
    ),
    intrinsic!(
        "__modifier_call_parts",
        "__staple_syntax_modifier_call_parts",
        modifier_call_parts
    ),
    intrinsic!(
        "__modifier_call_namespace_count",
        "__staple_syntax_modifier_call_namespace_count",
        modifier_call_namespace_count
    ),
    intrinsic!(
        "__modifier_call_name",
        "__staple_syntax_modifier_call_name",
        modifier_call_name
    ),
    intrinsic!(
        "__modifier_call_argument",
        "__staple_syntax_modifier_call_argument",
        modifier_call_argument
    ),
    intrinsic!(
        "__modifier_call_namespace",
        "__staple_syntax_modifier_call_namespace",
        modifier_call_namespace
    ),
    intrinsic!(
        "__classify_type_body",
        "__staple_syntax_classify_type_body",
        classify_type_body
    ),
    intrinsic!(
        "__type_body_parts",
        "__staple_syntax_type_body_parts",
        type_body_parts
    ),
    intrinsic!(
        "__delimiter_contents",
        "__staple_syntax_delimiter_contents",
        delimiter_contents
    ),
    intrinsic!(
        "__delimiter_syntax",
        "__staple_syntax_delimiter_syntax",
        delimiter_syntax
    ),
    intrinsic!(
        "__visibility_kind",
        "__staple_syntax_visibility_kind",
        visibility_kind
    ),
    intrinsic!(
        "__visibility_syntax",
        "__staple_syntax_visibility_syntax",
        visibility_syntax
    ),
    intrinsic!(
        "__punctuation_syntax",
        "__staple_syntax_punctuation_syntax",
        punctuation_syntax
    ),
    intrinsic!(
        "__modified_item_syntax",
        "__staple_syntax_modified_item_syntax",
        modified_item_syntax
    ),
    intrinsic!(
        "__type_declaration_syntax",
        "__staple_syntax_type_declaration_syntax",
        type_declaration_syntax
    ),
    intrinsic!(
        "__modifier_call_syntax",
        "__staple_syntax_modifier_call_syntax",
        modifier_call_syntax
    ),
    intrinsic!(
        "__type_body_syntax",
        "__staple_syntax_type_body_syntax",
        type_body_syntax
    ),
    intrinsic!(
        "__alias_body_syntax",
        "__staple_syntax_alias_body_syntax",
        alias_body_syntax
    ),
    intrinsic!(
        "__wrapper_body_syntax",
        "__staple_syntax_wrapper_body_syntax",
        wrapper_body_syntax
    ),
    intrinsic!(
        "__from_body_syntax",
        "__staple_syntax_from_body_syntax",
        from_body_syntax
    ),
    intrinsic!(
        "__type_parameter_parts",
        "__staple_syntax_type_parameter_parts",
        type_parameter_parts
    ),
    intrinsic!(
        "__type_parameter_name",
        "__staple_syntax_type_parameter_name",
        type_parameter_name
    ),
    intrinsic!(
        "__type_parameter_default",
        "__staple_syntax_type_parameter_default",
        type_parameter_default
    ),
    intrinsic!(
        "__type_parameter_declaration",
        "__staple_syntax_type_parameter_declaration",
        type_parameter_declaration
    ),
    intrinsic!(
        "__type_parameter_argument",
        "__staple_syntax_type_parameter_argument",
        type_parameter_argument
    ),
    intrinsic!(
        "__type_parameter_syntax",
        "__staple_syntax_type_parameter_syntax",
        type_parameter_syntax
    ),
    intrinsic!(
        "__type_parameters_parts",
        "__staple_syntax_type_parameters_parts",
        type_parameters_parts
    ),
    intrinsic!(
        "__type_parameters_count",
        "__staple_syntax_type_parameters_count",
        type_parameters_count
    ),
    intrinsic!(
        "__type_parameters_declaration",
        "__staple_syntax_type_parameters_declaration",
        type_parameters_declaration
    ),
    intrinsic!(
        "__type_parameters_arguments",
        "__staple_syntax_type_parameters_arguments",
        type_parameters_arguments
    ),
    intrinsic!(
        "__type_parameters_bounds",
        "__staple_syntax_type_parameters_bounds",
        type_parameters_bounds
    ),
    intrinsic!(
        "__type_parameters_parameter",
        "__staple_syntax_type_parameters_parameter",
        type_parameters_parameter
    ),
    intrinsic!(
        "__resource_dependency_syntax",
        "__staple_syntax_resource_dependency_syntax",
        resource_dependency_syntax
    ),
    intrinsic!(
        "__type_parameters_syntax",
        "__staple_syntax_type_parameters_syntax",
        type_parameters_syntax
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
        "__sequence_push" => "__staple_syntax_sequence_push",
        "__sequence_reverse" => "__staple_syntax_sequence_reverse",
        "__offset" => "__staple_syntax_offset",
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
        "__classify_expr" => "__staple_syntax_classify_expr",
        "__call_parts" => "__staple_syntax_call_parts",
        "__ident_text" => "__staple_syntax_ident_text",
        "__string_expr_text" => "__staple_syntax_string_expr_text",
        "__char_expr_char" => "__staple_syntax_char_expr_char",
        "__unstructured_expr" => "__staple_syntax_unstructured_expr",
        "__unstructured_expr_syntax" => "__staple_syntax_unstructured_expr_syntax",
        "__unstructured_type_syntax" => "__staple_syntax_unstructured_type_syntax",
        "__unstructured_pattern_syntax" => "__staple_syntax_unstructured_pattern_syntax",
        "__unstructured_item_syntax" => "__staple_syntax_unstructured_item_syntax",
        "__unstructured_modifier_syntax" => "__staple_syntax_unstructured_modifier_syntax",
        "__call_syntax" => "__staple_syntax_call_syntax",
        "__ident_syntax" => "__staple_syntax_ident_syntax",
        "__ident_spelling" => "__staple_syntax_ident_spelling",
        "__string_expr_syntax" => "__staple_syntax_string_expr_syntax",
        "__char_expr_syntax" => "__staple_syntax_char_expr_syntax",
        "__classify_type" => "__staple_syntax_classify_type",
        "__product_type_field_count" => "__staple_syntax_product_type_field_count",
        "__product_type_field" => "__staple_syntax_product_type_field",
        "__product_type_field_parts" => "__staple_syntax_product_type_field_parts",
        "__product_type_syntax" => "__staple_syntax_product_type_syntax",
        "__join_field_syntax" => "__staple_syntax_join_field_syntax",
        "__product_type_field_syntax" => "__staple_syntax_product_type_field_syntax",
        "__unstructured_type" => "__staple_syntax_unstructured_type",
        "__unstructured_pattern" => "__staple_syntax_unstructured_pattern",
        "__unstructured_item" => "__staple_syntax_unstructured_item",
        "__unstructured_modifier" => "__staple_syntax_unstructured_modifier",
        "__empty_syntax" => "__staple_syntax_empty_syntax",
        "__syntax_is_empty" => "__staple_syntax_syntax_is_empty",
        "__same_syntax" => "__staple_syntax_same_syntax",
        "__syntax_or_origin" => "__staple_syntax_syntax_or_origin",
        "__join_separated_syntax" => "__staple_syntax_join_separated_syntax",
        "__append_separator_syntax" => "__staple_syntax_append_separator_syntax",
        "__classify_pattern" => "__staple_syntax_classify_pattern",
        "__binding_pattern_name" => "__staple_syntax_binding_pattern_name",
        "__nominal_pattern_parts" => "__staple_syntax_nominal_pattern_parts",
        "__binding_pattern_syntax" => "__staple_syntax_binding_pattern_syntax",
        "__nominal_pattern_syntax" => "__staple_syntax_nominal_pattern_syntax",
        "__sequence_syntax" => "__staple_syntax_sequence_syntax",
        "__classify_item" => "__staple_syntax_classify_item",
        "__modified_item_parts" => "__staple_syntax_modified_item_parts",
        "__modified_item_modifier" => "__staple_syntax_modified_item_modifier",
        "__type_declaration_parts" => "__staple_syntax_type_declaration_parts",
        "__type_declaration_name" => "__staple_syntax_type_declaration_name",
        "__type_declaration_declared_type" => "__staple_syntax_type_declaration_declared_type",
        "__type_declaration_type_parameter_count" => "__staple_syntax_type_declaration_type_parameter_count",
        "__type_declaration_body" => "__staple_syntax_type_declaration_body",
        "__type_declaration_resource_count" => "__staple_syntax_type_declaration_resource_count",
        "__type_declaration_type_parameter" => "__staple_syntax_type_declaration_type_parameter",
        "__type_declaration_resource" => "__staple_syntax_type_declaration_resource",
        "__classify_modifier" => "__staple_syntax_classify_modifier",
        "__modifier_call_parts" => "__staple_syntax_modifier_call_parts",
        "__modifier_call_namespace" => "__staple_syntax_modifier_call_namespace",
        "__classify_type_body" => "__staple_syntax_classify_type_body",
        "__type_body_parts" => "__staple_syntax_type_body_parts",
        "__delimiter_contents" => "__staple_syntax_delimiter_contents",
        "__delimiter_syntax" => "__staple_syntax_delimiter_syntax",
        "__visibility_kind" => "__staple_syntax_visibility_kind",
        "__visibility_syntax" => "__staple_syntax_visibility_syntax",
        "__punctuation_syntax" => "__staple_syntax_punctuation_syntax",
        "__modified_item_syntax" => "__staple_syntax_modified_item_syntax",
        "__type_declaration_syntax" => "__staple_syntax_type_declaration_syntax",
        "__modifier_call_syntax" => "__staple_syntax_modifier_call_syntax",
        "__type_body_syntax" => "__staple_syntax_type_body_syntax",
        "__alias_body_syntax" => "__staple_syntax_alias_body_syntax",
        "__wrapper_body_syntax" => "__staple_syntax_wrapper_body_syntax",
        "__from_body_syntax" => "__staple_syntax_from_body_syntax",
        "__type_parameter_parts" => "__staple_syntax_type_parameter_parts",
        "__type_parameter_syntax" => "__staple_syntax_type_parameter_syntax",
        "__type_parameters_parts" => "__staple_syntax_type_parameters_parts",
        "__type_parameters_parameter" => "__staple_syntax_type_parameters_parameter",
        "__resource_dependency_syntax" => "__staple_syntax_resource_dependency_syntax",
        "__type_parameters_syntax" => "__staple_syntax_type_parameters_syntax",
        "__modifier_call_namespace_count" => "__staple_syntax_modifier_call_namespace_count",
        "__modifier_call_name" => "__staple_syntax_modifier_call_name",
        "__modifier_call_argument" => "__staple_syntax_modifier_call_argument",
        "__type_parameter_name" => "__staple_syntax_type_parameter_name",
        "__type_parameter_default" => "__staple_syntax_type_parameter_default",
        "__type_parameter_declaration" => "__staple_syntax_type_parameter_declaration",
        "__type_parameter_argument" => "__staple_syntax_type_parameter_argument",
        "__type_parameters_count" => "__staple_syntax_type_parameters_count",
        "__type_parameters_declaration" => "__staple_syntax_type_parameters_declaration",
        "__type_parameters_arguments" => "__staple_syntax_type_parameters_arguments",
        "__type_parameters_bounds" => "__staple_syntax_type_parameters_bounds",
        "__product_type_field_name" => "__staple_syntax_product_type_field_name",
        "__product_type_field_type" => "__staple_syntax_product_type_field_type",
        "__product_type_field_default" => "__staple_syntax_product_type_field_default",
        _ => "",
    }
}

/// The entry for a declared intrinsic, if the host provides it.
pub(crate) fn lookup(name: &str) -> Option<&'static Intrinsic> {
    INTRINSICS.iter().find(|intrinsic| intrinsic.name == name)
}

#[repr(C)]
struct Pair {
    first: u64,
    second: u64,
}

#[repr(C)]
struct Triple {
    first: u64,
    second: u64,
    third: u64,
}

#[repr(C)]
struct Quad {
    first: u64,
    second: u64,
    third: u64,
    fourth: u64,
}

#[repr(C)]
struct CountHandle {
    count: usize,
    handle: u64,
}

#[repr(C)]
struct HandleFlag {
    handle: u64,
    flag: u8,
}

#[repr(C)]
struct TypeDeclarationParts {
    name: u64,
    declared_type: u64,
    type_parameters: usize,
    body: u64,
    resources: usize,
}

#[repr(C)]
struct ModifierCallParts {
    namespace: usize,
    name: u64,
    argument: u64,
}

#[repr(C)]
struct TypeParametersParts {
    count: usize,
    declaration: u64,
    arguments: u64,
    bounds: u64,
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

/// A syntax fragment with no tokens.
extern "C" fn empty_syntax() -> u64 {
    arena::with_arena(|arena| {
        let syntax = Syntax::synthetic(SyntaxId(arena.next_syntax_id()), Span::Compiler);
        arena.insert_syntax(syntax).into()
    })
}

/// `1` when a syntax fragment has no tokens.
extern "C" fn syntax_is_empty(syntax: u64) -> u8 {
    u8::from(syntax_value(syntax).tokens().is_empty())
}

/// `1` when two syntax handles are the same handle.
extern "C" fn same_syntax(left: u64, right: u64) -> u8 {
    u8::from(left == right)
}

/// Returns `origin` when its non-trivia tokens match `candidate`'s, otherwise
/// `candidate`.
extern "C" fn syntax_or_origin(origin: u64, candidate: u64) -> u64 {
    let candidate_syntax = syntax_value(candidate);
    if origin != 0 {
        let origin_syntax = syntax_value(origin);
        let original: Vec<&str> = origin_syntax
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        let rebuilt: Vec<&str> = candidate_syntax
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        if original == rebuilt {
            return origin;
        }
    }
    candidate
}

/// Appends one separated element, with a separator between elements.
extern "C" fn join_separated_syntax(joined: u64, element: u64, separator: u64, first: u8) -> u64 {
    let element_syntax = syntax_value(element);
    if first != 0 {
        return element;
    }
    let joined_syntax = syntax_value(joined);
    let separator_syntax = syntax_value(separator);
    let mut tokens = Vec::new();
    append_syntax_tokens(&mut tokens, &joined_syntax, None);
    append_syntax_tokens(&mut tokens, &separator_syntax, None);
    append_syntax_tokens(&mut tokens, &element_syntax, None);
    finish_syntax(0, None, tokens)
}

/// Appends a trailing separator.
extern "C" fn append_separator_syntax(joined: u64, separator: u64) -> u64 {
    let joined_syntax = syntax_value(joined);
    let separator_syntax = syntax_value(separator);
    let mut tokens = Vec::new();
    append_syntax_tokens(&mut tokens, &joined_syntax, None);
    append_syntax_tokens(&mut tokens, &separator_syntax, None);
    finish_syntax(0, None, tokens)
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


// --- node views -------------------------------------------------------------

/// The `Expr` alternative a fragment names. `0` is `Ident`, `1` `CallExpr`,
/// `2` `StringExpr`, `3` `CharExpr`, and `4` `UnstructuredExpr`.
extern "C" fn classify_expr(syntax: u64) -> u8 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    match staple_syntax::parse_expression_fragment(&syntax, &mut next) {
        Ok(Expression::Name(_)) => 0,
        Ok(Expression::Call(_)) => 1,
        Ok(Expression::String(_)) => 2,
        Ok(Expression::Char(_)) => 3,
        _ => 4,
    }
}

/// The callee and argument of a call expression.
extern "C" fn call_parts(syntax: u64) -> Pair {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Expression::Call(call) =
        staple_syntax::parse_expression_fragment(&syntax, &mut next)
            .expect("call_parts requires a call expression")
    else {
        panic!("call_parts requires a call expression");
    };
    arena::with_arena(|arena| Pair {
        first: arena.insert_syntax(call.callee.syntax().clone()).into(),
        second: arena.insert_syntax(call.argument.syntax().clone()).into(),
    })
}

/// The spelling of an identifier, as a text handle.
extern "C" fn ident_text(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let text = match staple_syntax::parse_expression_fragment(&syntax, &mut next) {
        Ok(Expression::Name(name)) => name.name,
        _ => syntax
            .tokens()
            .iter()
            .find(|token| !token.kind.is_trivia())
            .map(|token| token.text.clone())
            .unwrap_or_default(),
    };
    arena::with_arena(|arena| arena.insert_text(text.into_bytes()).into())
}

/// The decoded contents of a string-literal expression, as a text handle.
extern "C" fn string_expr_text(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Expression::String(string) =
        staple_syntax::parse_expression_fragment(&syntax, &mut next)
            .expect("string_expr_text requires a string expression")
    else {
        panic!("string_expr_text requires a string expression");
    };
    let text = string_literal::decode(&string.literal).unwrap_or(string.literal);
    arena::with_arena(|arena| arena.insert_text(text.into_bytes()).into())
}

/// The code point of a character-literal expression.
extern "C" fn char_expr_char(syntax: u64) -> u32 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Expression::Char(character) =
        staple_syntax::parse_expression_fragment(&syntax, &mut next)
            .expect("char_expr_char requires a character expression")
    else {
        panic!("char_expr_char requires a character expression");
    };
    string_literal::decode_char(&character.literal)
        .expect("a parsed character literal decodes")
        as u32
}

/// The opaque handle for an unstructured expression fragment.
extern "C" fn unstructured_expr(syntax: u64) -> u64 {
    syntax
}

extern "C" fn unstructured_type(syntax: u64) -> u64 {
    syntax
}

extern "C" fn unstructured_pattern(syntax: u64) -> u64 {
    syntax
}

extern "C" fn unstructured_item(syntax: u64) -> u64 {
    syntax
}

extern "C" fn unstructured_modifier(syntax: u64) -> u64 {
    syntax
}

fn single_space() -> SyntaxToken {
    SyntaxToken {
        kind: TokenKind::Whitespace,
        text: " ".to_owned(),
        span: 0..0,
        origin: None,
        hygiene: None,
    }
}

/// Builds a new syntax fragment from token parts. The origin supplies the
/// span and context; a constructed node has none.
fn syntax_from_parts(origin: Option<&Syntax>, parts: &[(u64, Option<Vec<SyntaxToken>>)]) -> u64 {
    let mut tokens = Vec::new();
    for (handle, literal) in parts {
        if let Some(literal) = literal {
            tokens.extend(literal.iter().cloned());
        } else {
            let syntax = syntax_value(*handle);
            append_syntax_tokens(&mut tokens, &syntax, None);
        }
    }
    arena::with_arena(|arena| {
        let mut syntax = Syntax::synthetic(
            SyntaxId(arena.next_syntax_id()),
            origin.map_or(Span::Compiler, |origin| origin.span.clone()),
        );
        syntax.tokens = Arc::from(tokens);
        syntax.token_range = 0..syntax.tokens.len();
        if let Some(origin) = origin {
            syntax.definition_module = origin.definition_module;
            syntax.expansion_mark = origin.expansion_mark;
        }
        arena.insert_syntax(syntax).into()
    })
}

/// The tokens between two child syntax slices of the same origin.
fn between_tokens(origin: &Syntax, left: &Syntax, right: &Syntax) -> Option<Vec<SyntaxToken>> {
    if !origin.shares_token_stream(left) || !origin.shares_token_stream(right) {
        return None;
    }
    let start = left.token_range.end;
    let end = right.token_range.start;
    (start <= end).then(|| origin.tokens()[start..end].to_vec())
}

/// Rebuilds a call expression, returning the origin when its children are
/// unchanged.
extern "C" fn call_syntax(origin: u64, callee: u64, argument: u64) -> u64 {
    let callee_syntax = syntax_value(callee);
    let argument_syntax = syntax_value(argument);
    let mut origin_syntax = None;
    if origin != 0 {
        let syntax = syntax_value(origin);
        let mut next = 0;
        if let Ok(Expression::Call(call)) =
            staple_syntax::parse_expression_fragment(&syntax, &mut next)
        {
            if call.callee.syntax() == &callee_syntax && call.argument.syntax() == &argument_syntax {
                return origin;
            }
            let separator = between_tokens(&syntax, call.callee.syntax(), call.argument.syntax());
            return arena::with_arena(|arena| {
                let mut tokens = Vec::new();
                append_syntax_tokens(&mut tokens, &callee_syntax, None);
                match &separator {
                    Some(separator) => tokens.extend(separator.iter().cloned()),
                    None => tokens.push(single_space()),
                }
                append_syntax_tokens(&mut tokens, &argument_syntax, None);
                let mut result = Syntax::synthetic(
                    SyntaxId(arena.next_syntax_id()),
                    syntax.span.clone(),
                );
                result.tokens = Arc::from(tokens);
                result.token_range = 0..result.tokens.len();
                result.definition_module = syntax.definition_module;
                result.expansion_mark = syntax.expansion_mark;
                arena.insert_syntax(result).into()
            });
        }
        origin_syntax = Some(syntax);
    }
    let origin_ref = origin_syntax.as_ref();
    syntax_from_parts(
        origin_ref,
        &[
            (callee, None),
            (0, Some(vec![single_space()])),
            (argument, None),
        ],
    )
}

/// Rebuilds an identifier, returning the origin when its spelling is
/// unchanged.
extern "C" fn ident_syntax(origin: u64, text: u64) -> u64 {
    let text = String::from_utf8(text_bytes(text)).expect("identifier text is UTF-8");
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    if let Some(syntax) = &origin_syntax {
        let spelling: String = syntax
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        if spelling == text {
            return origin;
        }
    }
    let tokens = staple_syntax::lex(&text);
    assert!(
        matches!(tokens.as_slice(), [token] if token.kind == TokenKind::Identifier),
        "`{text}` is not an identifier"
    );
    arena::with_arena(|arena| {
        let mut syntax = Syntax::synthetic(
            SyntaxId(arena.next_syntax_id()),
            origin_syntax.as_ref().map_or(Span::Compiler, |origin| origin.span.clone()),
        );
        syntax.tokens = Arc::from(tokens);
        syntax.token_range = 0..syntax.tokens.len();
        if let Some(origin) = &origin_syntax {
            syntax.definition_module = origin.definition_module;
            syntax.expansion_mark = origin.expansion_mark;
        }
        arena.insert_syntax(syntax).into()
    })
}

/// Rebuilds a string-literal expression, returning the origin when its value
/// is unchanged.
extern "C" fn string_expr_syntax(origin: u64, text: u64) -> u64 {
    let text = String::from_utf8(text_bytes(text)).expect("string text is UTF-8");
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    if let Some(syntax) = &origin_syntax {
        let mut next = 0;
        if let Ok(Expression::String(string)) =
            staple_syntax::parse_expression_fragment(syntax, &mut next)
            && string_literal::decode(&string.literal).ok().as_deref() == Some(&text)
        {
            return origin;
        }
    }
    let literal = string_literal::encode(&text);
    let tokens = staple_syntax::lex(&literal);
    arena::with_arena(|arena| {
        let mut syntax = Syntax::synthetic(
            SyntaxId(arena.next_syntax_id()),
            origin_syntax.as_ref().map_or(Span::Compiler, |origin| origin.span.clone()),
        );
        syntax.tokens = Arc::from(tokens);
        syntax.token_range = 0..syntax.tokens.len();
        if let Some(origin) = &origin_syntax {
            syntax.definition_module = origin.definition_module;
            syntax.expansion_mark = origin.expansion_mark;
        }
        arena.insert_syntax(syntax).into()
    })
}

/// Rebuilds a character-literal expression, returning the origin when its
/// value is unchanged.
extern "C" fn char_expr_syntax(origin: u64, value: u32) -> u64 {
    let character = char::from_u32(value).expect("a valid character code point");
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    if let Some(syntax) = &origin_syntax {
        let mut next = 0;
        if let Ok(Expression::Char(existing)) =
            staple_syntax::parse_expression_fragment(syntax, &mut next)
            && string_literal::decode_char(&existing.literal).ok() == Some(character)
        {
            return origin;
        }
    }
    let literal = string_literal::encode_char(character);
    let tokens = staple_syntax::lex(&literal);
    arena::with_arena(|arena| {
        let mut syntax = Syntax::synthetic(
            SyntaxId(arena.next_syntax_id()),
            origin_syntax.as_ref().map_or(Span::Compiler, |origin| origin.span.clone()),
        );
        syntax.tokens = Arc::from(tokens);
        syntax.token_range = 0..syntax.tokens.len();
        if let Some(origin) = &origin_syntax {
            syntax.definition_module = origin.definition_module;
            syntax.expansion_mark = origin.expansion_mark;
        }
        arena.insert_syntax(syntax).into()
    })
}


// --- types and patterns -----------------------------------------------------

/// The `Type` alternative a fragment names: `0` `ProductType`, `1`
/// `UnstructuredType`.
extern "C" fn classify_type(syntax: u64) -> u8 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    match staple_syntax::parse_type_fragment(&syntax, false, &mut next) {
        Ok(ty) if is_structured_product_type(&ty) => 0,
        _ => 1,
    }
}

/// The number of fields in a structured product type.
extern "C" fn product_type_field_count(syntax: u64) -> usize {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(ty) = staple_syntax::parse_type_fragment(&syntax, false, &mut next) else {
        panic!("product_type_field_count requires a product type");
    };
    let staple_syntax::Type::Product(product) = ty else {
        panic!("product_type_field_count requires a product type");
    };
    product.elements.len()
}

/// One field of a structured product type, as a fragment.
extern "C" fn product_type_field(syntax: u64, index: usize) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(staple_syntax::Type::Product(product)) =
        staple_syntax::parse_type_fragment(&syntax, false, &mut next)
    else {
        panic!("product_type_field requires a product type");
    };
    let field = product
        .elements
        .get(index)
        .expect("product type field index is in range");
    arena::with_arena(|arena| arena.insert_syntax(field.syntax.clone()).into())
}

/// A product field's name, type and default. Zero means the field has none.
extern "C" fn product_type_field_parts(syntax: u64) -> Triple {
    let syntax = syntax_value(syntax);
    let text = format!("({})", syntax.text());
    let wrapped = arena::with_arena(|arena| {
        let mut next = arena.next_syntax_id();
        arena.insert_syntax(lex_syntax_from_text(&text, &mut next)).into()
    });
    let wrapped = syntax_value(wrapped);
    let mut next = 0;
    let Ok(staple_syntax::Type::Product(product)) =
        staple_syntax::parse_type_fragment(&wrapped, false, &mut next)
    else {
        panic!("product_type_field_parts requires one product field");
    };
    let field = product
        .elements
        .first()
        .expect("a product field has an element");
    arena::with_arena(|arena| {
        let name = field.name.as_ref().map_or(0, |name| {
            let text = name.clone();
            let mut next = arena.next_syntax_id();
            let syntax = lex_syntax_from_text(&text, &mut next);
            u64::from(arena.insert_syntax(syntax))
        });
        let ty = arena.insert_syntax(field.ty.syntax().clone()).into();
        let default = field.default.as_ref().map_or(0, |default| {
            u64::from(arena.insert_syntax(default.syntax().clone()))
        });
        Triple {
            first: name,
            second: ty,
            third: default,
        }
    })
}

/// The syntax without its leading and trailing trivia.
fn trimmed_syntax(syntax: &Syntax) -> Syntax {
    let tokens = syntax.tokens();
    let first = tokens
        .iter()
        .position(|token| !token.kind.is_trivia())
        .unwrap_or(0);
    let last = tokens
        .iter()
        .rposition(|token| !token.kind.is_trivia())
        .map_or(first, |last| last + 1);
    syntax_slice(syntax, first, last)
}

fn product_field(syntax: &Syntax) -> staple_syntax::TypeElement {
    let text = format!("({})", syntax.text());
    let mut next = 0;
    let wrapped = lex_syntax_from_text(&text, &mut next);
    let Ok(staple_syntax::Type::Product(product)) =
        staple_syntax::parse_type_fragment(&wrapped, false, &mut next)
    else {
        panic!("a product field parses as a one-element product");
    };
    product
        .elements
        .into_iter()
        .next()
        .expect("a product field has an element")
}

extern "C" fn product_type_field_name(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let field = product_field(&syntax);
    arena::with_arena(|arena| match field.name {
        Some(name) => {
            let mut next = arena.next_syntax_id();
            let syntax = lex_syntax_from_text(&name, &mut next);
            arena.insert_syntax(syntax).into()
        }
        None => {
            let empty = Syntax::synthetic(SyntaxId(arena.next_syntax_id()), Span::Compiler);
            arena.insert_syntax(empty).into()
        }
    })
}

extern "C" fn product_type_field_type(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let field = product_field(&syntax);
    let ty = trimmed_syntax(field.ty.syntax());
    arena::with_arena(|arena| arena.insert_syntax(ty).into())
}

extern "C" fn product_type_field_default(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let field = product_field(&syntax);
    arena::with_arena(|arena| match field.default {
        Some(default) => {
            let default = trimmed_syntax(default.syntax());
            arena.insert_syntax(default).into()
        }
        None => {
            let empty = Syntax::synthetic(SyntaxId(arena.next_syntax_id()), Span::Compiler);
            arena.insert_syntax(empty).into()
        }
    })
}

/// Appends one product field, with a comma between fields.
extern "C" fn join_field_syntax(joined: u64, field: u64, first: u8) -> u64 {
    let field_syntax = syntax_value(field);
    if first != 0 {
        return field;
    }
    let joined_syntax = syntax_value(joined);
    let mut tokens = Vec::new();
    append_syntax_tokens(&mut tokens, &joined_syntax, None);
    tokens.extend(literal_tokens(", "));
    append_syntax_tokens(&mut tokens, &field_syntax, None);
    finish_syntax(0, None, tokens)
}

/// Rebuilds a product type from its fields, or returns its origin when
/// unchanged.
extern "C" fn product_type_syntax(origin: u64, fields: u64) -> u64 {
    let fields_syntax = syntax_value(fields);
    let origin_syntax = (origin != 0)
        .then(|| syntax_value(origin))
        .filter(|syntax| !syntax.tokens().is_empty());
    if let Some(syntax) = &origin_syntax {
        let tokens = syntax.tokens();
        let first = tokens
            .iter()
            .position(|token| !token.kind.is_trivia())
            .unwrap_or(0);
        let last = tokens
            .iter()
            .rposition(|token| !token.kind.is_trivia())
            .unwrap_or(first);
        let original: Vec<&str> = if first < last {
            tokens[first + 1..last]
                .iter()
                .filter(|token| !token.kind.is_trivia())
                .map(|token| token.text.as_str())
                .collect()
        } else {
            Vec::new()
        };
        let rebuilt: Vec<&str> = fields_syntax
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        if original == rebuilt {
            return origin;
        }
    }
    let open = SyntaxToken {
        kind: TokenKind::LParen,
        text: "(".to_owned(),
        span: 0..0,
        origin: None,
        hygiene: None,
    };
    let close = SyntaxToken {
        kind: TokenKind::RParen,
        text: ")".to_owned(),
        span: 0..0,
        origin: None,
        hygiene: None,
    };
    syntax_from_parts(
        origin_syntax.as_ref(),
        &[(0, Some(vec![open])), (fields, None), (0, Some(vec![close]))],
    )
}

/// A product field's syntax, rebuilt from its parts.
extern "C" fn product_type_field_syntax(name: u64, ty: u64, default: u64) -> u64 {
    let name_syntax = (name != 0).then(|| syntax_value(name));
    let default_syntax = (default != 0).then(|| syntax_value(default));
    let mut parts = Vec::new();
    if name_syntax
        .as_ref()
        .is_some_and(|syntax| !syntax.tokens().is_empty())
    {
        parts.push((name, None));
        parts.push((0, Some(vec![SyntaxToken {
            kind: TokenKind::Colon,
            text: ":".to_owned(),
            span: 0..0,
            origin: None,
            hygiene: None,
        }])));
        parts.push((0, Some(vec![single_space()])));
    }
    parts.push((ty, None));
    if default_syntax
        .as_ref()
        .is_some_and(|syntax| !syntax.tokens().is_empty())
    {
        parts.push((0, Some(vec![single_space()])));
        parts.push((0, Some(vec![SyntaxToken {
            kind: TokenKind::Equals,
            text: "=".to_owned(),
            span: 0..0,
            origin: None,
            hygiene: None,
        }])));
        parts.push((0, Some(vec![single_space()])));
        parts.push((default, None));
    }
    syntax_from_parts(None, &parts)
}

/// The opaque handle for an unstructured type fragment.
extern "C" fn unstructured_type_from_syntax(syntax: u64) -> u64 {
    syntax
}

extern "C" fn unstructured_pattern_from_syntax(syntax: u64) -> u64 {
    syntax
}

extern "C" fn unstructured_item_from_syntax(syntax: u64) -> u64 {
    syntax
}

extern "C" fn unstructured_modifier_from_syntax(syntax: u64) -> u64 {
    syntax
}

/// The `Pattern` alternative a fragment names: `0` `BindingPattern`, `1`
/// `NominalPattern`, `2` `UnstructuredPattern`.
extern "C" fn classify_pattern(syntax: u64) -> u8 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    match staple_syntax::parse_pattern_fragment(&syntax, false, &mut next) {
        Ok(staple_syntax::Pattern::Binding(_)) => 0,
        Ok(staple_syntax::Pattern::Nominal(_)) => 1,
        _ => 2,
    }
}

/// The name of a binding pattern, as an identifier fragment.
extern "C" fn binding_pattern_name(syntax: u64) -> u64 {
    syntax
}

/// A nominal pattern's name and argument.
extern "C" fn nominal_pattern_parts(syntax: u64) -> Pair {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(staple_syntax::Pattern::Nominal(pattern)) =
        staple_syntax::parse_pattern_fragment(&syntax, false, &mut next)
    else {
        panic!("nominal_pattern_parts requires a nominal pattern");
    };
    let argument = pattern.argument.syntax().clone();
    let name = {
        let tokens = syntax.tokens();
        let end = argument
            .token_range
            .start
            .saturating_sub(syntax.token_range.start);
        tokens[..end]
            .iter()
            .rposition(|token| {
                !token.kind.is_trivia() && token.kind == TokenKind::Identifier
            })
            .map(|index| syntax_slice(&syntax, index, index + 1))
            .unwrap_or_else(|| syntax_slice(&syntax, 0, 0))
    };
    arena::with_arena(|arena| Pair {
        first: arena.insert_syntax(name).into(),
        second: arena.insert_syntax(argument).into(),
    })
}

/// Rebuilds a binding pattern from its name.
extern "C" fn binding_pattern_syntax(origin: u64, name: u64) -> u64 {
    let name_syntax = syntax_value(name);
    if origin != 0 {
        let origin_syntax = syntax_value(origin);
        if origin_syntax.tokens() == name_syntax.tokens() {
            return origin;
        }
    }
    name
}

/// Rebuilds a nominal pattern from its name and argument.
extern "C" fn nominal_pattern_syntax(origin: u64, name: u64, argument: u64) -> u64 {
    let name_syntax = syntax_value(name);
    let argument_syntax = syntax_value(argument);
    let mut origin_syntax = None;
    if origin != 0 {
        let syntax = syntax_value(origin);
        let mut next = 0;
        if let Ok(staple_syntax::Pattern::Nominal(pattern)) =
            staple_syntax::parse_pattern_fragment(&syntax, false, &mut next)
        {
            let origin_name = pattern
                .argument
                .syntax()
                .token_range
                .start
                .saturating_sub(syntax.token_range.start);
            let name_end = origin_name.min(syntax.tokens().len());
            let same_name = name_syntax
                .tokens()
                .iter()
                .filter(|token| !token.kind.is_trivia())
                .map(|token| token.text.as_str())
                .eq(syntax.tokens()[..name_end]
                    .iter()
                    .filter(|token| !token.kind.is_trivia())
                    .map(|token| token.text.as_str()));
            if same_name && pattern.argument.syntax() == &argument_syntax {
                return origin;
            }
            let separator = between_tokens(&syntax, &name_syntax, pattern.argument.syntax());
            origin_syntax = Some((syntax, separator));
        } else {
            origin_syntax = Some((syntax, None));
        }
    }
    let (origin_ref, separator) = match &origin_syntax {
        Some((syntax, separator)) => (Some(syntax), separator.clone()),
        None => (None, None),
    };
    let mut parts = vec![(name, None)];
    match separator {
        Some(separator) => parts.push((0, Some(separator))),
        None => parts.push((0, Some(vec![single_space()]))),
    }
    parts.push((argument, None));
    syntax_from_parts(origin_ref, &parts)
}


/// Rebuilds one sequence cell from its element and spliced rest. The origin is
/// the suffix starting at this cell; unchanged children return it.
extern "C" fn sequence_syntax(origin: u64, first: u64, rest: u64) -> u64 {
    let first_syntax = syntax_value(first);
    let rest_syntax = syntax_value(rest);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let Some(origin_syntax) = origin_syntax else {
        return syntax_from_parts(None, &[(first, None), (rest, None)]);
    };
    let origin_text = origin_syntax.text();
    let first_text = first_syntax.text();
    let rest_text = rest_syntax.text();
    if origin_text == format!("{first_text}{rest_text}") {
        return origin;
    }
    if origin_text.starts_with(&first_text) && origin_text.ends_with(&rest_text) {
        let first_end = first_text.len();
        let rest_start = origin_text.len() - rest_text.len();
        if first_end <= rest_start {
            let mut offset = 0;
            let mut separator = Vec::new();
            for token in origin_syntax.tokens() {
                let start = offset;
                offset += token.text.len();
                if start >= first_end && offset <= rest_start {
                    separator.push(token.clone());
                }
            }
            let mut parts = vec![(first, None), (0, Some(separator)), (rest, None)];
            if parts[1].1.as_ref().is_some_and(Vec::is_empty) {
                parts.remove(1);
            }
            return syntax_from_parts(Some(&origin_syntax), &parts);
        }
    }
    syntax_from_parts(Some(&origin_syntax), &[(first, None), (rest, None)])
}


// --- items, modifiers and type bodies --------------------------------------

/// The `Item` alternative a fragment names: `0` `ModifiedItem`, `1`
/// `TypeDeclarationItem`, `2` `UnstructuredItem`.
extern "C" fn classify_item(syntax: u64) -> u8 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    match staple_syntax::parse_item_fragment(&syntax, &mut next) {
        Ok(Item::Modified(_)) => 0,
        Ok(Item::TypeDeclaration(_)) => 1,
        _ => 2,
    }
}

/// The modifiers and item of a modified item.
extern "C" fn modified_item_parts(syntax: u64) -> CountHandle {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::Modified(modified)) = staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("modified_item_parts requires a modified item");
    };
    arena::with_arena(|arena| CountHandle {
        count: modified.modifiers.len(),
        handle: arena.insert_syntax(modified.item.syntax().clone()).into(),
    })
}

/// One modifier of a modified item.
extern "C" fn modified_item_modifier(syntax: u64, index: usize) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::Modified(modified)) = staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("modified_item_modifier requires a modified item");
    };
    let modifier = modified
        .modifiers
        .get(index)
        .expect("modifier index is in range");
    arena::with_arena(|arena| arena.insert_syntax(modifier.syntax.clone()).into())
}

/// A type declaration's name, declared type, parameter count, body and
/// resource-dependency count.
extern "C" fn type_declaration_parts(syntax: u64) -> TypeDeclarationParts {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_parts requires a type declaration");
    };
    let names: Vec<String> = declaration
        .type_parameters
        .iter()
        .flat_map(staple_syntax::TypeParameterPattern::names)
        .map(str::to_owned)
        .collect();
    let declared_text = if names.is_empty() {
        declaration.name.clone()
    } else {
        format!("{}<{}>", declaration.name, names.join(", "))
    };
    arena::with_arena(|arena| {
        let name = arena.insert_syntax(declaration.name_syntax.clone()).into();
        let mut next = arena.next_syntax_id();
        let declared = lex_syntax_from_text(&declared_text, &mut next);
        let declared = arena.insert_syntax(declared).into();
        let body = declaration
            .body
            .as_ref()
            .map_or(0, |body| u64::from(arena.insert_syntax(body.syntax.clone())));
        let resources = declaration
            .resource_dependencies
            .as_ref()
            .map_or(0, |dependencies| dependencies.resources.len());
        TypeDeclarationParts {
            name,
            declared_type: declared,
            type_parameters: names.len(),
            body,
            resources,
        }
    })
}

/// A type declaration's name.
extern "C" fn type_declaration_name(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_name requires a type declaration");
    };
    arena::with_arena(|arena| arena.insert_syntax(declaration.name_syntax.clone()).into())
}

/// A type declaration's declared type, as a fragment.
extern "C" fn type_declaration_declared_type(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_declared_type requires a type declaration");
    };
    let names: Vec<String> = declaration
        .type_parameters
        .iter()
        .flat_map(staple_syntax::TypeParameterPattern::names)
        .map(str::to_owned)
        .collect();
    let text = if names.is_empty() {
        declaration.name.clone()
    } else {
        format!("{}<{}>", declaration.name, names.join(", "))
    };
    arena::with_arena(|arena| {
        let mut next = arena.next_syntax_id();
        arena.insert_syntax(lex_syntax_from_text(&text, &mut next)).into()
    })
}

/// The number of type parameters of a type declaration.
extern "C" fn type_declaration_type_parameter_count(syntax: u64) -> usize {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_type_parameter_count requires a type declaration");
    };
    declaration.type_parameters.len()
}

/// A type declaration's body, or an empty fragment when it has none.
extern "C" fn type_declaration_body(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_body requires a type declaration");
    };
    arena::with_arena(|arena| match &declaration.body {
        Some(body) => arena.insert_syntax(body.syntax.clone()).into(),
        None => {
            let syntax = Syntax::synthetic(SyntaxId(arena.next_syntax_id()), Span::Compiler);
            arena.insert_syntax(syntax).into()
        }
    })
}

/// The number of resource dependencies of a type declaration.
extern "C" fn type_declaration_resource_count(syntax: u64) -> usize {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_resource_count requires a type declaration");
    };
    declaration
        .resource_dependencies
        .as_ref()
        .map_or(0, |dependencies| dependencies.resources.len())
}

/// One type parameter of a type declaration, as its name fragment.
extern "C" fn type_declaration_type_parameter(syntax: u64, index: usize) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_type_parameter requires a type declaration");
    };
    let name = declaration
        .type_parameters
        .get(index)
        .and_then(|parameter| parameter.names().first().copied())
        .unwrap_or("")
        .to_owned();
    arena::with_arena(|arena| {
        let mut next = arena.next_syntax_id();
        let syntax = lex_syntax_from_text(&name, &mut next);
        arena.insert_syntax(syntax).into()
    })
}

/// One resource dependency of a type declaration: its type and mutability.
extern "C" fn type_declaration_resource(syntax: u64, index: usize) -> HandleFlag {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(Item::TypeDeclaration(declaration)) =
        staple_syntax::parse_item_fragment(&syntax, &mut next)
    else {
        panic!("type_declaration_resource requires a type declaration");
    };
    let resource = declaration
        .resource_dependencies
        .as_ref()
        .and_then(|dependencies| dependencies.resources.get(index))
        .expect("resource dependency index is in range");
    arena::with_arena(|arena| HandleFlag {
        handle: arena.insert_syntax(resource.value_type.syntax().clone()).into(),
        flag: u8::from(resource.mutable),
    })
}

/// The `Modifier` alternative a fragment names: `0` `ModifierCall`, `1`
/// `UnstructuredModifier`.
extern "C" fn classify_modifier(syntax: u64) -> u8 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    match staple_syntax::parse_modifier_fragment(&syntax, &mut next) {
        Ok(invocation) if invocation.doc.is_none() => 0,
        _ => 1,
    }
}

/// A modifier call's namespace count, name and argument (zero when absent).
extern "C" fn modifier_call_parts(syntax: u64) -> ModifierCallParts {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(invocation) = staple_syntax::parse_modifier_fragment(&syntax, &mut next) else {
        panic!("modifier_call_parts requires a modifier");
    };
    let tokens = invocation.syntax.tokens();
    let segments = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !token.kind.is_trivia())
        .skip(1)
        .take_while(|(_, token)| token.kind != TokenKind::LParen)
        .filter(|(_, token)| token.kind != TokenKind::Dot)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let namespace = invocation
        .namespace
        .as_deref()
        .map(|path| path.split('.').collect::<Vec<_>>())
        .unwrap_or_default();
    let name_index = segments
        .get(namespace.len())
        .copied()
        .unwrap_or(0);
    arena::with_arena(|arena| {
        let name = syntax_slice(&invocation.syntax, name_index, name_index + 1);
        let name = arena.insert_syntax(name).into();
        let argument = invocation.argument.as_ref().map_or(0, |argument| {
            let tokens = argument.syntax.tokens();
            let first = tokens
                .iter()
                .position(|token| !token.kind.is_trivia())
                .unwrap_or(0);
            let last = tokens
                .iter()
                .rposition(|token| !token.kind.is_trivia())
                .unwrap_or(first);
            let inner = syntax_slice(&argument.syntax, first + 1, last);
            u64::from(arena.insert_syntax(inner))
        });
        ModifierCallParts {
            namespace: namespace.len(),
            name,
            argument,
        }
    })
}

/// The number of namespace segments of a modifier call.
extern "C" fn modifier_call_namespace_count(syntax: u64) -> usize {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(invocation) = staple_syntax::parse_modifier_fragment(&syntax, &mut next) else {
        panic!("modifier_call_namespace_count requires a modifier");
    };
    invocation
        .namespace
        .as_deref()
        .map_or(0, |path| path.split('.').count())
}

/// The name of a modifier call.
extern "C" fn modifier_call_name(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(invocation) = staple_syntax::parse_modifier_fragment(&syntax, &mut next) else {
        panic!("modifier_call_name requires a modifier");
    };
    let tokens = invocation.syntax.tokens();
    let segments = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !token.kind.is_trivia())
        .skip(1)
        .take_while(|(_, token)| token.kind != TokenKind::LParen)
        .filter(|(_, token)| token.kind != TokenKind::Dot)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let namespace = invocation
        .namespace
        .as_deref()
        .map(|path| path.split('.').count())
        .unwrap_or(0);
    let name_index = segments.get(namespace).copied().unwrap_or(0);
    arena::with_arena(|arena| {
        arena
            .insert_syntax(syntax_slice(&invocation.syntax, name_index, name_index + 1))
            .into()
    })
}

/// The argument of a modifier call, or an empty fragment when absent.
extern "C" fn modifier_call_argument(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(invocation) = staple_syntax::parse_modifier_fragment(&syntax, &mut next) else {
        panic!("modifier_call_argument requires a modifier");
    };
    arena::with_arena(|arena| match &invocation.argument {
        Some(argument) => {
            let tokens = argument.syntax.tokens();
            let first = tokens
                .iter()
                .position(|token| !token.kind.is_trivia())
                .unwrap_or(0);
            let last = tokens
                .iter()
                .rposition(|token| !token.kind.is_trivia())
                .unwrap_or(first);
            arena
                .insert_syntax(syntax_slice(&argument.syntax, first + 1, last))
                .into()
        }
        None => {
            let empty = Syntax::synthetic(SyntaxId(arena.next_syntax_id()), Span::Compiler);
            arena.insert_syntax(empty).into()
        }
    })
}

/// One namespace segment of a modifier call.
extern "C" fn modifier_call_namespace(syntax: u64, index: usize) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(invocation) = staple_syntax::parse_modifier_fragment(&syntax, &mut next) else {
        panic!("modifier_call_namespace requires a modifier");
    };
    let tokens = invocation.syntax.tokens();
    let segments = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !token.kind.is_trivia())
        .skip(1)
        .take_while(|(_, token)| token.kind != TokenKind::LParen)
        .filter(|(_, token)| token.kind != TokenKind::Dot)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let segment = segments
        .get(index)
        .copied()
        .expect("namespace index is in range");
    arena::with_arena(|arena| {
        arena
            .insert_syntax(syntax_slice(&invocation.syntax, segment, segment + 1))
            .into()
    })
}

/// The `TypeBody` alternative a fragment names: `0` alias, `1` wrapper, `2`
/// from, `3` opaque.
extern "C" fn classify_type_body(syntax: u64) -> u8 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    match staple_syntax::parse_type_body_fragment(&syntax, &mut next) {
        Ok(body) => match body.kind {
            staple_syntax::TypeBodyKind::Alias => 0,
            staple_syntax::TypeBodyKind::Wrapper => 1,
            staple_syntax::TypeBodyKind::From => 2,
            staple_syntax::TypeBodyKind::Opaque => 3,
        },
        Err(_) => 3,
    }
}

/// A type body's representation visibility and underlying type (zero when
/// absent).
extern "C" fn type_body_parts(syntax: u64) -> Pair {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok(body) = staple_syntax::parse_type_body_fragment(&syntax, &mut next) else {
        panic!("type_body_parts requires a type body");
    };
    arena::with_arena(|arena| {
        let representation = arena.insert_syntax(body.representation.syntax.clone()).into();
        let underlying = body.underlying.as_ref().map_or(0, |ty| {
            u64::from(arena.insert_syntax(ty.syntax().clone()))
        });
        Pair {
            first: representation,
            second: underlying,
        }
    })
}

/// The contents of a delimited fragment, without the delimiters.
extern "C" fn delimiter_contents(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let tokens = syntax.tokens();
    let first = tokens
        .iter()
        .position(|token| !token.kind.is_trivia())
        .expect("a delimiter has an opening token");
    let inner = match tokens[first].kind {
        TokenKind::Operator if tokens[first].text == "<" => {
            crate::macro_expand::syntax_ops::matching_angle_close(tokens, first)
        }
        _ => crate::macro_expand::syntax_ops::matching_close(tokens, first),
    }
    .expect("a delimited fragment has a closing delimiter");
    arena::with_arena(|arena| {
        arena
            .insert_syntax(syntax_slice(&syntax, first + 1, inner))
            .into()
    })
}

/// Rebuilds a delimited fragment. `kind` is `0` parenthesized, `1` bracketed,
/// `2` angled, `3` braced.
extern "C" fn delimiter_syntax(origin: u64, kind: u8, contents: u64) -> u64 {
    let contents_syntax = syntax_value(contents);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    if let Some(syntax) = &origin_syntax {
        let tokens = syntax.tokens();
        let first = tokens
            .iter()
            .position(|token| !token.kind.is_trivia())
            .unwrap_or(0);
        let last = tokens
            .iter()
            .rposition(|token| !token.kind.is_trivia())
            .unwrap_or(first);
        let inner = syntax_slice(syntax, first + 1, last);
        let original: Vec<&str> = inner
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        let rebuilt: Vec<&str> = contents_syntax
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        if original == rebuilt {
            return origin;
        }
    }
    let (open, close) = match kind {
        0 => ("(", ")"),
        1 => ("[", "]"),
        2 => ("<", ">"),
        _ => ("{", "}"),
    };
    let open = SyntaxToken {
        kind: match kind {
            0 => TokenKind::LParen,
            1 => TokenKind::LBracket,
            2 => TokenKind::Operator,
            _ => TokenKind::LBrace,
        },
        text: open.to_owned(),
        span: 0..0,
        origin: None,
        hygiene: None,
    };
    let close = SyntaxToken {
        kind: match kind {
            0 => TokenKind::RParen,
            1 => TokenKind::RBracket,
            2 => TokenKind::Operator,
            _ => TokenKind::RBrace,
        },
        text: close.to_owned(),
        span: 0..0,
        origin: None,
        hygiene: None,
    };
    syntax_from_parts(
        origin_syntax.as_ref(),
        &[(0, Some(vec![open])), (contents, None), (0, Some(vec![close]))],
    )
}

/// The visibility kind a fragment names: `0` private, `1` package, `2`
/// public.
extern "C" fn visibility_kind(syntax: u64) -> u8 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    match staple_syntax::parse_expression_fragment(&syntax, &mut next) {
        Ok(Expression::VisibilityArgument(visibility)) => match visibility.kind {
            staple_syntax::VisibilityKind::Private => 0,
            staple_syntax::VisibilityKind::Package => 1,
            staple_syntax::VisibilityKind::Public => 2,
        },
        _ => 0,
    }
}

/// The syntax of a visibility kind.
extern "C" fn visibility_syntax(kind: u8) -> u64 {
    let text = match kind {
        1 => "pub(package)",
        2 => "pub",
        _ => "",
    };
    arena::with_arena(|arena| {
        let mut next = arena.next_syntax_id();
        arena.insert_syntax(lex_syntax_from_text(text, &mut next)).into()
    })
}

/// The syntax of a punctuation kind: `0` comma, `1` equals, `2` fat arrow.
extern "C" fn punctuation_syntax(kind: u8) -> u64 {
    let text = match kind {
        0 => ",",
        1 => "=",
        _ => "=>",
    };
    arena::with_arena(|arena| {
        let mut next = arena.next_syntax_id();
        arena.insert_syntax(lex_syntax_from_text(text, &mut next)).into()
    })
}


fn literal_tokens(text: &str) -> Vec<SyntaxToken> {
    staple_syntax::lex(text)
}

/// Builds a fragment from literal and child parts, returning the origin when
/// the rebuilt text is unchanged.
fn finish_syntax(origin: u64, origin_syntax: Option<&Syntax>, tokens: Vec<SyntaxToken>) -> u64 {
    if let Some(syntax) = origin_syntax {
        let rebuilt: Vec<&str> = tokens
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        let original: Vec<&str> = syntax
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        if original == rebuilt {
            return origin;
        }
    }
    arena::with_arena(|arena| {
        let mut syntax = Syntax::synthetic(
            SyntaxId(arena.next_syntax_id()),
            origin_syntax.map_or(Span::Compiler, |origin| origin.span.clone()),
        );
        syntax.tokens = Arc::from(tokens);
        syntax.token_range = 0..syntax.tokens.len();
        if let Some(origin) = origin_syntax {
            syntax.definition_module = origin.definition_module;
            syntax.expansion_mark = origin.expansion_mark;
        }
        arena.insert_syntax(syntax).into()
    })
}

/// Rebuilds a modified item.
extern "C" fn modified_item_syntax(origin: u64, modifiers: u64, item: u64) -> u64 {
    let modifiers_syntax = syntax_value(modifiers);
    let item_syntax = syntax_value(item);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = Vec::new();
    append_syntax_tokens(&mut tokens, &modifiers_syntax, None);
    if !modifiers_syntax.tokens().is_empty() && !item_syntax.tokens().is_empty() {
        tokens.push(single_space());
    }
    append_syntax_tokens(&mut tokens, &item_syntax, None);
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

/// Rebuilds a type declaration item.
extern "C" fn type_declaration_syntax(
    origin: u64,
    name: u64,
    declared_type: u64,
    type_parameters: u64,
    body: u64,
    resources: u64,
) -> u64 {
    let name = syntax_value(name);
    let type_parameters = syntax_value(type_parameters);
    let body = syntax_value(body);
    let resources = syntax_value(resources);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = literal_tokens("type ");
    append_syntax_tokens(&mut tokens, &name, None);
    if !type_parameters.tokens().is_empty() {
        tokens.push(SyntaxToken {
            kind: TokenKind::Operator,
            text: "<".to_owned(),
            span: 0..0,
            origin: None,
            hygiene: None,
        });
        append_syntax_tokens(&mut tokens, &type_parameters, None);
        tokens.push(SyntaxToken {
            kind: TokenKind::Operator,
            text: ">".to_owned(),
            span: 0..0,
            origin: None,
            hygiene: None,
        });
    }
    if !body.tokens().is_empty() {
        tokens.push(single_space());
        append_syntax_tokens(&mut tokens, &body, None);
    }
    if !resources.tokens().is_empty() {
        tokens.extend(literal_tokens(" with { "));
        append_syntax_tokens(&mut tokens, &resources, None);
        tokens.extend(literal_tokens(" }"));
    }
    let _ = declared_type;
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

/// Rebuilds a modifier call.
extern "C" fn modifier_call_syntax(
    origin: u64,
    namespace: u64,
    name: u64,
    argument: u64,
) -> u64 {
    let namespace = syntax_value(namespace);
    let name = syntax_value(name);
    let argument = syntax_value(argument);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = literal_tokens("@");
    append_syntax_tokens(&mut tokens, &namespace, None);
    if !namespace.tokens().is_empty() {
        tokens.extend(literal_tokens("."));
    }
    append_syntax_tokens(&mut tokens, &name, None);
    if !argument.tokens().is_empty() {
        tokens.extend(literal_tokens("("));
        append_syntax_tokens(&mut tokens, &argument, None);
        tokens.extend(literal_tokens(")"));
    }
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

/// Rebuilds a type body.
extern "C" fn type_body_syntax(origin: u64, kind: u8, representation: u64, underlying: u64) -> u64 {
    let representation = syntax_value(representation);
    let underlying = syntax_value(underlying);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = match kind {
        0 => literal_tokens("= "),
        1 => literal_tokens("wrap "),
        2 => literal_tokens("from "),
        _ => literal_tokens("opaque"),
    };
    if matches!(kind, 1 | 2) && !representation.tokens().is_empty() {
        append_syntax_tokens(&mut tokens, &representation, None);
        tokens.push(single_space());
    }
    if kind != 3 {
        append_syntax_tokens(&mut tokens, &underlying, None);
    }
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

extern "C" fn alias_body_syntax(origin: u64, element: u64) -> u64 {
    let element = syntax_value(element);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = literal_tokens("= ");
    append_syntax_tokens(&mut tokens, &element, None);
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

extern "C" fn wrapper_body_syntax(origin: u64, representation: u64, element: u64) -> u64 {
    let representation = syntax_value(representation);
    let element = syntax_value(element);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = literal_tokens("wrap ");
    if !representation.tokens().is_empty() {
        append_syntax_tokens(&mut tokens, &representation, None);
        tokens.push(single_space());
    }
    append_syntax_tokens(&mut tokens, &element, None);
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

extern "C" fn from_body_syntax(origin: u64, representation: u64, element: u64) -> u64 {
    let representation = syntax_value(representation);
    let element = syntax_value(element);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = literal_tokens("from ");
    if !representation.tokens().is_empty() {
        append_syntax_tokens(&mut tokens, &representation, None);
        tokens.push(single_space());
    }
    append_syntax_tokens(&mut tokens, &element, None);
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

/// A type parameter's name, default, declaration and argument.
extern "C" fn type_parameter_parts(syntax: u64) -> Quad {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok((parameter, _)) = staple_syntax::parse_type_parameter_prefix(&syntax, &mut next) else {
        panic!("type_parameter_parts requires a type parameter");
    };
    let name = parameter
        .pattern
        .names()
        .first()
        .copied()
        .unwrap_or("")
        .to_owned();
    let mut name_syntax = parameter.pattern.syntax().clone();
    if let Some(index) = name_syntax
        .tokens()
        .iter()
        .position(|token| token.text == name)
    {
        name_syntax.token_range =
            name_syntax.token_range.start + index..name_syntax.token_range.start + index + 1;
    }
    let argument = format!("{name}");
    arena::with_arena(|arena| {
        let name = arena.insert_syntax(name_syntax).into();
        let default = parameter.default.as_ref().map_or(0, |ty| {
            u64::from(arena.insert_syntax(ty.syntax().clone()))
        });
        let declaration = arena.insert_syntax(parameter.syntax.clone()).into();
        let mut next = arena.next_syntax_id();
        let argument = lex_syntax_from_text(&argument, &mut next);
        let argument = arena.insert_syntax(argument).into();
        Quad {
            first: name,
            second: default,
            third: declaration,
            fourth: argument,
        }
    })
}

fn type_parameter(syntax: &Syntax) -> staple_syntax::TypeParameter {
    let mut next = 0;
    staple_syntax::parse_type_parameter_prefix(syntax, &mut next)
        .expect("a type parameter parses")
        .0
}

extern "C" fn type_parameter_name(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let parameter = type_parameter(&syntax);
    let name = parameter
        .pattern
        .names()
        .first()
        .copied()
        .unwrap_or("")
        .to_owned();
    let mut name_syntax = parameter.pattern.syntax().clone();
    if let Some(index) = name_syntax
        .tokens()
        .iter()
        .position(|token| token.text == name)
    {
        name_syntax.token_range =
            name_syntax.token_range.start + index..name_syntax.token_range.start + index + 1;
    }
    arena::with_arena(|arena| arena.insert_syntax(name_syntax).into())
}

extern "C" fn type_parameter_default(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let parameter = type_parameter(&syntax);
    arena::with_arena(|arena| match &parameter.default {
        Some(ty) => arena.insert_syntax(ty.syntax().clone()).into(),
        None => {
            let empty = Syntax::synthetic(SyntaxId(arena.next_syntax_id()), Span::Compiler);
            arena.insert_syntax(empty).into()
        }
    })
}

extern "C" fn type_parameter_declaration(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let parameter = type_parameter(&syntax);
    arena::with_arena(|arena| arena.insert_syntax(parameter.syntax.clone()).into())
}

extern "C" fn type_parameter_argument(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let parameter = type_parameter(&syntax);
    let name = parameter
        .pattern
        .names()
        .first()
        .copied()
        .unwrap_or("")
        .to_owned();
    arena::with_arena(|arena| {
        let mut next = arena.next_syntax_id();
        let argument = lex_syntax_from_text(&name, &mut next);
        arena.insert_syntax(argument).into()
    })
}

extern "C" fn type_parameter_syntax(
    origin: u64,
    name: u64,
    default: u64,
    declaration: u64,
    argument: u64,
) -> u64 {
    let name = syntax_value(name);
    let default = syntax_value(default);
    let argument = syntax_value(argument);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    if declaration != 0 {
        let declaration_syntax = syntax_value(declaration);
        if !declaration_syntax.tokens().is_empty() {
            if let Some(origin_syntax) = &origin_syntax {
                let original: Vec<&str> = origin_syntax
                    .tokens()
                    .iter()
                    .filter(|token| !token.kind.is_trivia())
                    .map(|token| token.text.as_str())
                    .collect();
                let declared: Vec<&str> = declaration_syntax
                    .tokens()
                    .iter()
                    .filter(|token| !token.kind.is_trivia())
                    .map(|token| token.text.as_str())
                    .collect();
                if original == declared {
                    return origin;
                }
            }
            return declaration;
        }
    }
    if let Some(syntax) = &origin_syntax {
        let parameter = type_parameter(syntax);
        let origin_name = parameter
            .pattern
            .names()
            .first()
            .copied()
            .unwrap_or("");
        let name_text: String = name
            .tokens()
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.text.as_str())
            .collect();
        let origin_default = parameter.default.as_ref().map(|ty| ty.syntax());
        let default_matches = match (origin_default, default.tokens().is_empty()) {
            (Some(origin), false) => {
                origin.tokens().iter().filter(|token| !token.kind.is_trivia()).map(|token| token.text.as_str())
                    .eq(default.tokens().iter().filter(|token| !token.kind.is_trivia()).map(|token| token.text.as_str()))
            }
            (None, true) => true,
            _ => false,
        };
        if origin_name == name_text && default_matches {
            let _ = (declaration, argument);
            return origin;
        }
    }
    let mut tokens = Vec::new();
    append_syntax_tokens(&mut tokens, &name, None);
    tokens.extend(literal_tokens(": "));
    append_syntax_tokens(&mut tokens, &argument, None);
    if !default.tokens().is_empty() {
        tokens.extend(literal_tokens(" = "));
        append_syntax_tokens(&mut tokens, &default, None);
    }
    let _ = declaration;
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
}

/// One type parameter of a type-parameter list.
extern "C" fn type_parameters_parameter(syntax: u64, index: usize) -> u64 {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok((parameters, _)) = staple_syntax::parse_type_parameters_prefix(&syntax, &mut next)
    else {
        panic!("type_parameters_parameter requires type parameters");
    };
    let parameter = parameters
        .parameters
        .get(index)
        .expect("type parameter index is in range");
    arena::with_arena(|arena| arena.insert_syntax(parameter.syntax.clone()).into())
}

/// A resource dependency's syntax: `mut Type` or `Type`.
extern "C" fn resource_dependency_syntax(resource_type: u64, mutable: u8) -> u64 {
    let resource_type = syntax_value(resource_type);
    let mut tokens = Vec::new();
    if mutable != 0 {
        tokens.extend(literal_tokens("mut "));
    }
    append_syntax_tokens(&mut tokens, &resource_type, None);
    finish_syntax(0, None, tokens)
}

/// A type-parameter list's parameter count, declaration, arguments and bounds.
extern "C" fn type_parameters_parts(syntax: u64) -> TypeParametersParts {
    let syntax = syntax_value(syntax);
    let mut next = 0;
    let Ok((parameters, _)) = staple_syntax::parse_type_parameters_prefix(&syntax, &mut next)
    else {
        panic!("type_parameters_parts requires type parameters");
    };
    let arguments = {
        let tokens = parameters.syntax.tokens();
        let mut angles = 0usize;
        let mut close = tokens.len();
        for (index, token) in tokens.iter().enumerate() {
            match token.kind {
                TokenKind::Operator if token.text == "<" => angles += 1,
                TokenKind::Operator if token.text == ">" => {
                    angles = angles.saturating_sub(1);
                    if angles == 0 {
                        close = index + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        syntax_slice(&parameters.syntax, 0, close)
    };
    arena::with_arena(|arena| TypeParametersParts {
        count: parameters.parameters.len(),
        declaration: arena.insert_syntax(parameters.syntax.clone()).into(),
        arguments: arena.insert_syntax(arguments).into(),
        bounds: arena.insert_syntax(parameters.bounds.clone()).into(),
    })
}

fn type_parameters(syntax: &Syntax) -> staple_syntax::TypeParameters {
    let mut next = 0;
    staple_syntax::parse_type_parameters_prefix(syntax, &mut next)
        .expect("type parameters parse")
        .0
}

extern "C" fn type_parameters_count(syntax: u64) -> usize {
    let syntax = syntax_value(syntax);
    type_parameters(&syntax).parameters.len()
}

extern "C" fn type_parameters_declaration(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let parameters = type_parameters(&syntax);
    arena::with_arena(|arena| arena.insert_syntax(parameters.syntax.clone()).into())
}

extern "C" fn type_parameters_arguments(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let parameters = type_parameters(&syntax);
    let arguments = {
        let tokens = parameters.syntax.tokens();
        let mut angles = 0usize;
        let mut close = tokens.len();
        for (index, token) in tokens.iter().enumerate() {
            match token.kind {
                TokenKind::Operator if token.text == "<" => angles += 1,
                TokenKind::Operator if token.text == ">" => {
                    angles = angles.saturating_sub(1);
                    if angles == 0 {
                        close = index + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        syntax_slice(&parameters.syntax, 0, close)
    };
    arena::with_arena(|arena| arena.insert_syntax(arguments).into())
}

extern "C" fn type_parameters_bounds(syntax: u64) -> u64 {
    let syntax = syntax_value(syntax);
    let parameters = type_parameters(&syntax);
    arena::with_arena(|arena| arena.insert_syntax(parameters.bounds.clone()).into())
}

extern "C" fn type_parameters_syntax(
    origin: u64,
    parameters: u64,
    declaration: u64,
    arguments: u64,
    bounds: u64,
) -> u64 {
    let parameters = syntax_value(parameters);
    let bounds = syntax_value(bounds);
    let origin_syntax = (origin != 0).then(|| syntax_value(origin));
    let mut tokens = literal_tokens("<");
    append_syntax_tokens(&mut tokens, &parameters, None);
    tokens.extend(literal_tokens(">"));
    if !bounds.tokens().is_empty() {
        tokens.push(single_space());
        append_syntax_tokens(&mut tokens, &bounds, None);
    }
    let _ = (declaration, arguments);
    finish_syntax(origin, origin_syntax.as_ref(), tokens)
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
    fn every_entry_has_the_prefix_and_no_duplicates() {
        let mut names = std::collections::HashSet::new();
        for intrinsic in INTRINSICS {
            assert!(intrinsic.symbol.starts_with("__staple_syntax_"));
            assert!(names.insert(intrinsic.name), "duplicate {}", intrinsic.name);
        }
    }

    #[test]
    fn the_node_view_intrinsics_are_in_the_table() {
        for name in [
            "__classify_expr",
            "__call_parts",
            "__ident_text",
            "__ident_spelling",
            "__call_syntax",
            "__ident_syntax",
            "__classify_type",
            "__product_type_field_count",
            "__product_type_field",
            "__product_type_syntax",
            "__classify_pattern",
            "__nominal_pattern_parts",
            "__sequence_syntax",
            "__classify_item",
            "__modified_item_parts",
            "__type_declaration_parts",
            "__classify_modifier",
            "__modifier_call_parts",
            "__classify_type_body",
            "__type_body_parts",
            "__delimiter_contents",
            "__delimiter_syntax",
            "__visibility_kind",
            "__punctuation_syntax",
            "__type_parameter_parts",
            "__type_parameters_parts",
            "__same_syntax",
            "__syntax_or_origin",
            "__join_separated_syntax",
            "__append_separator_syntax",
            "__join_field_syntax",
            "__sequence_reverse",
        ] {
            assert!(lookup(name).is_some(), "{name}");
        }
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
