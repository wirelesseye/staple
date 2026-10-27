//! Structural `Default` for products and the `@derive_default` modifier.
//!
//! A fixed product implements `Default` when every element does, unless its
//! fields declare defaults. `@derive_default` builds a nominal type's default
//! from its representation, keeping explicit field defaults, including those
//! of nested products.

mod common;

// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use staple_syntax::Diagnostic;
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn check_result(source: &str) -> Result<staple_compiler::TypedModule, Vec<Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let resolved = NameResolver::new().resolve_program(program)?;
    TypeChecker::new().check(resolved)
}

fn check(source: &str) -> staple_compiler::TypedModule {
    check_result(source).unwrap_or_else(|errors| panic!("check: {errors:?}"))
}

fn check_errors(source: &str) -> Vec<String> {
    check_result(source)
        .expect_err("source should fail checking")
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

fn lower(module: &staple_compiler::TypedModule) -> LoweredModule {
    Lowerer::new()
        .lower(module)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

/// Builds and runs a program, returning its standard output.
#[cfg(unix)]
fn run(source: &str) -> String {
    let module = check(source);
    let lowered = lower(&module);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-derive-default-{}-{nonce}",
        std::process::id()
    ));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(&lowered, &object, None)
        .unwrap_or_else(|errors| panic!("emit: {errors:?}"));
    let link = common::linker(&[&object], &base).output().unwrap();
    assert!(
        link.status.success(),
        "link: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let output = Command::new(&base).output().unwrap();
    let _ = std::fs::remove_file(object);
    let _ = std::fs::remove_file(base);
    assert!(
        output.status.success(),
        "execution: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn products_default_each_element_in_order() {
    let output = run(concat!(
        "use std.io.println\n",
        "use std.fmt.Formatter\n",
        "def repeated: <T where Default T> () -> (T ^ 3) = <T where Default T> () => default ()\n",
        "def main = () => {\n",
        "    let pair: (I32, Bool) = default ()\n",
        "    let named: (x: I32, label: String) = default ()\n",
        "    let nested: ((I32, Bool), String, F64) = default ()\n",
        "    let unit: () = default ()\n",
        "    let three: (I32 ^ 3) = repeated ()\n",
        "    println (Formatter.debug pair)\n",
        "    println (Formatter.debug named)\n",
        "    println (Formatter.debug nested)\n",
        "    println (Formatter.debug three)\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(
        output,
        "(0, False)\n(x: 0, label: \"\")\n((0, False), \"\", 0)\n(0, 0, 0)\n"
    );
}

#[test]
fn products_require_every_element_to_be_default() {
    let messages = check_errors(concat!(
        "type Local wrap I32\n",
        "let invalid: (Local, I32) = default ()\n",
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("no trait implementation")),
        "{messages:?}"
    );
}

#[test]
fn shared_values_do_not_default_structurally() {
    let messages = check_errors(concat!(
        "use std.shared.(Shared)\n",
        "let invalid: Shared<I32> = default ()\n",
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("no trait implementation")),
        "{messages:?}"
    );
}

#[cfg(unix)]
#[test]
fn derived_defaults_keep_explicit_field_defaults() {
    let output = run(concat!(
        "use std.io.println\n",
        "@derive_default\n",
        "type Point wrap (x: I32 = 5, y: I32)\n",
        "@derive_default\n",
        "type Config from (window: (width: I32 = 800, height: I32), title: String = \"app\")\n",
        "@derive_default\n",
        "type Text from (String, x: I32 = 3)\n",
        "def main = () => {\n",
        "    let Point (x, y) = Point :: default ()\n",
        "    let Config (window, title) = Config :: default ()\n",
        "    let Text (text, offset) = Text :: default ()\n",
        "    println \"${x} ${y}\"\n",
        "    println \"${window.width} ${window.height} ${title}\"\n",
        "    println \"[${text}] ${offset}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "5 0\n800 0 app\n[] 3\n");
}

#[cfg(unix)]
#[test]
fn derived_defaults_cover_plain_singleton_and_generic_types() {
    let output = run(concat!(
        "use std.io.println\n",
        "@derive_default\n",
        "type Plain wrap I32\n",
        "@derive_default\n",
        "type Pair from (I32, Bool)\n",
        "@derive_default\n",
        "type Enabled\n",
        "@derive_default\n",
        "type Box<T> wrap (value: T, count: I32 = 1)\n",
        "@derive_default\n",
        "type Two<A, B> from (first: A, second: (B, I32), flag: Bool = True)\n",
        "@derive_default\n",
        "type Triple<T> wrap (items: (T ^ 3), tag: I32 = 7)\n",
        "def main = () => {\n",
        "    let Plain plain = Plain :: default ()\n",
        "    let (left, right) = Pair :: default ()\n",
        "    let enabled: Enabled = default ()\n",
        "    let Box (value, count) = Box<String> :: default ()\n",
        "    let (first, second, flag) = Two<I32, Bool> :: default ()\n",
        "    let Triple (items, tag) = Triple<I32> :: default ()\n",
        "    println \"${plain} ${left} ${right}\"\n",
        "    println \"[${value}] ${count}\"\n",
        "    println \"${first} ${second.0} ${second.1} ${flag}\"\n",
        "    println \"${items.0} ${items.2} ${tag}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "0 0 False\n[] 1\n0 False 0 True\n0 0 7\n");
}

#[cfg(unix)]
#[test]
fn derived_defaults_ignore_a_local_default_trait() {
    // The generated implementation names `std.default.Default` hygienically,
    // so a caller's own `Default` neither receives it nor fills its fields.
    let output = run(concat!(
        "use std.io.println\n",
        "use std.default.(derive_default)\n",
        "trait Default T {\n",
        "    default: () -> T\n",
        "}\n",
        "impl Default I32 { default = () => 42 }\n",
        "@derive_default\n",
        "type Point wrap (x: I32 = 5, y: I32)\n",
        "def main = () => {\n",
        "    let Point (x, y) = std.default.Default.default.<Point> ()\n",
        "    let local: I32 = Default.default ()\n",
        "    println \"${x} ${y} ${local}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "5 0 42\n");
}

#[cfg(unix)]
#[test]
fn derived_defaults_resolve_field_types_at_the_call_site() {
    // `std.default` sees `std.syntax.ParseError`; the spliced field type must
    // still name the caller's own `ParseError`.
    let output = run(concat!(
        "use std.io.println\n",
        "type ParseError wrap I32\n",
        "impl Default ParseError { default = () => ParseError 7 }\n",
        "@derive_default\n",
        "type Holder wrap (error: ParseError, n: I32 = 1)\n",
        "def main = () => {\n",
        "    let Holder (ParseError code, n) = Holder :: default ()\n",
        "    println \"${code} ${n}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "7 1\n");
}

#[test]
fn derived_defaults_require_default_fields() {
    let messages = check_errors(concat!(
        "type Local wrap I32\n",
        "@derive_default\n",
        "type Wrapper wrap (local: Local, count: I32 = 1)\n",
        "let invalid: Wrapper = default ()\n",
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("no trait implementation")),
        "{messages:?}"
    );
}

#[test]
fn derived_defaults_reject_aliases() {
    let messages = check_errors("@derive_default\ntype Alias = I32\n");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("derive_default item is unsupported")),
        "{messages:?}"
    );
}

#[cfg(unix)]
#[test]
fn macros_inspect_product_type_fields() {
    // Each field contributes its name, or `_`, then `=` when it declares a
    // default and `()` when its type is itself a structured product.
    let output = run(concat!(
        "use std.syntax.*\n",
        "use std.process.(panic)\n",
        "use std.io.println\n",
        "use std.fmt.Formatter\n",
        "def describe_fields = fields: Sequence<ProductTypeField> => Sequence<Expr> :: match fields {\n",
        "    Sequence () => Sequence (),\n",
        "    Sequence (first, rest) => {\n",
        "        let name: Expr = match first {\n",
        "            ProductTypeField (Some (Ident name), _, _) => StringExpr name,\n",
        "            ProductTypeField (None, _, _) => Expr :: parse_quote { \"_\" },\n",
        "        }\n",
        "        let marker: Expr = match first {\n",
        "            ProductTypeField (_, _, Some _) => Expr :: parse_quote { \"=\" },\n",
        "            ProductTypeField (_, ProductType _, None) => Expr :: parse_quote { \"()\" },\n",
        "            ProductTypeField (_, UnstructuredType, None) => Expr :: parse_quote { \"\" },\n",
        "        }\n",
        "        Sequence (first: name, rest: Sequence (first: marker, rest: describe_fields rest))\n",
        "    },\n",
        "}\n",
        "macro @describe: (Parenthesized<Ident<String>>; Item) -> Sequence<Item> = (binding; item) => {\n",
        "    let binding = BindingPattern binding\n",
        "    match item {\n",
        "    TypeDeclarationItem (_, _, _, Some (WrapperBody (_, ProductType fields)), _) => {\n",
        "        let parts = describe_fields fields\n",
        "        Sequence (first: item, rest: Sequence (first: parse_quote { let $binding = ($parts...) }, rest: Sequence ()))\n",
        "    },\n",
        "    TypeDeclarationItem (_, _, _, Some (WrapperBody (_, UnstructuredType)), _) => Sequence (\n",
        "        first: item,\n",
        "        rest: Sequence (first: parse_quote { let $binding = \"unstructured\" }, rest: Sequence ()),\n",
        "    ),\n",
        "    _ => panic \"unsupported\",\n",
        "    }\n",
        "}\n",
        "@describe(point)\n",
        "type Point wrap (x: I32 = 0, String, y: (I32, I32))\n",
        "@describe(slots)\n",
        "type Slots wrap (I32; String)\n",
        "@describe(plain)\n",
        "type Plain wrap I32\n",
        "def main = () => {\n",
        "    println (Formatter.debug point)\n",
        "    println slots\n",
        "    println plain\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(
        output,
        "(\"x\", \"=\", \"_\", \"\", \"y\", \"()\")\nunstructured\nunstructured\n"
    );
}

#[cfg(unix)]
#[test]
fn macro_parameters_accept_type_subcategories() {
    let output = run(concat!(
        "use std.syntax.*\n",
        "use std.io.println\n",
        "macro kind = ty: ProductType => Expr :: parse_quote { \"product\" }\n",
        "macro kind = ty: UnstructuredType => Expr :: parse_quote { \"other\" }\n",
        "def main = () => {\n",
        "    println (kind (I32, Bool))\n",
        "    println (kind I32)\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "product\nother\n");
}
