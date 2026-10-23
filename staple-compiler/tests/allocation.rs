//! Exercises the low-level `std.alloc` surface and the consuming `List`
//! iterator directly.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn lower(source: &str, dynamic: bool) -> LoweredModule {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let resolved = NameResolver::new()
        .resolve_program(program)
        .unwrap_or_else(|errors| panic!("resolve: {errors:?}"));
    let typed = TypeChecker::new()
        .check(resolved)
        .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    Lowerer::new()
        .with_dictionary_pass_generic_calls(dynamic)
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

#[cfg(unix)]
fn run(source: &str) -> String {
    run_mode(source, false)
}

#[cfg(unix)]
fn run_mode(source: &str, dynamic: bool) -> String {
    let lowered = lower(source, dynamic);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-allocation-{}-{nonce}",
        std::process::id()
    ));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(&lowered, &object, None)
        .unwrap_or_else(|errors| panic!("emit: {errors:?}"));
    let link = Command::new("cc")
        .arg(&object)
        .arg("-o")
        .arg(&base)
        .output()
        .unwrap();
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
fn alloc_primitives_allocate_read_write_and_release() {
    let source = concat!(
        "use std.alloc.*\n",
        "use std.ptr.*\n",
        "use std.cinterop.*\n",
        "extern \"c\" { printf: (RawPtr<CChar>, ...) -> I32 }\n",
        "type Tag wrap I32\n",
        "impl Drop Tag { cleanup = Tag id => { printf (c_string \"drop %d\\n\", id); () } }\n",
        "def main = () => () :: {\n",
        "  let layout = MemLayout.of.<Tag> ()\n",
        "  printf (c_string \"layout %d %d %d\\n\", layout.size, layout.align, if (Gc.needs_drop.<Tag> ()) { 1 } else { 0 })\n",
        "  let pointer: RawMutPtr<Tag> = Gc.allocate layout as RawMutPtr<Tag>\n",
        "  RawMutPtr.store pointer (Tag 7)\n",
        "  let value = RawMutPtr.unsafe_read pointer\n",
        "  printf (c_string \"read %d\\n\", match value { Tag id => id })\n",
        "  RawMutPtr.store pointer value\n",
        "  RawMutPtr.unsafe_drop_in_place pointer\n",
        "  Gc.unsafe_release (pointer as RawOpaqueMutPtr)\n",
        "  ()\n",
        "}\n",
        "main ()\n",
    );
    assert_eq!(run(source), "layout 4 4 1\nread 7\ndrop 7\n");
}

#[cfg(unix)]
#[test]
fn list_into_iter_drops_only_unyielded_elements_on_early_break() {
    let source = concat!(
        "use std.list.List\n",
        "use std.iterator.(IntoIterator, Iterator)\n",
        "use std.iterator.IterStep.(Done, Yield)\n",
        "use std.cinterop.*\n",
        "use std.ptr.*\n",
        "extern \"c\" { printf: (RawPtr<CChar>, ...) -> I32 }\n",
        "type Tag wrap I32\n",
        "impl ! Copy Tag {}\n",
        "impl Drop Tag { cleanup = Tag id => { printf (c_string \"drop %d\\n\", id); () } }\n",
        "def main = () => () :: {\n",
        "  let mut list: List<Tag> = List.new ()\n",
        "  List.push list (Tag 1)\n",
        "  List.push list (Tag 2)\n",
        "  List.push list (Tag 3)\n",
        "  let mut iter = IntoIterator.into_iterator list\n",
        "  match Iterator.next iter {\n",
        "    Yield (value, rest) => {\n",
        "      iter = rest\n",
        "      printf (c_string \"yield %d\\n\", match value { Tag id => id })\n",
        "    },\n",
        "    Done _ => (),\n",
        "  }\n",
        "  ()\n",
        "}\n",
        "main ()\n",
    );
    // The yielded value drops at the end of its arm. The iterator releases
    // the allocation and drops the two elements it never handed out.
    assert_eq!(run(source), "yield 1\ndrop 1\ndrop 2\ndrop 3\n");
}

#[cfg(unix)]
#[test]
fn library_handles_move_replace_extract_and_drop_payloads_once() {
    let source = concat!(
        "use std.unique.Unique\n",
        "use std.shared.Shared\n",
        "use std.cinterop.*\n",
        "use std.ptr.*\n",
        "extern \"c\" { printf: (RawPtr<CChar>, ...) -> I32 }\n",
        "type Tag wrap I32\n",
        "impl !Copy Tag {}\n",
        "impl Drop Tag { cleanup = Tag id => { printf (c_string \"drop %d\\n\", id); () } }\n",
        "def exercise = () => () :: {\n",
        "  let mut owner = Unique (Tag 1)\n",
        "  let previous = Unique.replace (owner, Tag 2)\n",
        "  drop previous\n",
        "  let payload: Tag = owner.*\n",
        "  drop payload\n",
        "  let raw = Unique.unsafe_into_raw (Unique (Tag 3))\n",
        "  let restored = Unique.unsafe_from_raw raw\n",
        "  let payload = Unique.into_inner restored\n",
        "  drop payload\n",
        "  let empty = Unique ()\n",
        "  drop empty\n",
        "  let mut shared = Shared (Tag 4)\n",
        "  let alias = shared\n",
        "  let previous = Shared.replace (shared, Tag 5)\n",
        "  drop previous\n",
        "  let id = match alias { Tag id => id }\n",
        "  printf (c_string \"alias %d\\n\", id)\n",
        "  ()\n",
        "}\n",
        "exercise ()\n",
    );
    for dynamic in [false, true] {
        assert_eq!(run_mode(source, dynamic), "drop 1\ndrop 2\ndrop 3\ndrop 4\nalias 5\n");
    }
}

#[cfg(unix)]
#[test]
fn dropping_an_unsized_opaque_pointee_needs_no_layout_or_load() {
    // Opaque payloads have no drop glue. This also checks that the primitive
    // accepts ?Sized without trying to load a by-value representation.
    let source = concat!(
        "use std.ptr.RawMutPtr\n",
        "type Token opaque\n",
        "let pointer: RawMutPtr<Token> = RawMutPtr.null ()\n",
        "RawMutPtr.unsafe_drop_in_place pointer\n",
    );
    assert_eq!(run(source), "");
}

#[cfg(unix)]
#[test]
fn user_types_with_handle_names_keep_ordinary_wrapper_behavior() {
    let source = concat!(
        "use std.alloc.Gc\n",
        "use std.ptr.RawMutPtr\n",
        "use std.cinterop.*\n",
        "use std.ptr.RawPtr\n",
        "extern \"c\" { printf: (RawPtr<CChar>, ...) -> I32 }\n",
        "type Unique<T> wrap RawMutPtr<T>\n",
        "type Shared<T> wrap RawMutPtr<T>\n",
        "def exercise = () => () :: {\n",
        "  let pointer: RawMutPtr<I32> = RawMutPtr.null ()\n",
        "  let owner = Unique pointer\n",
        "  let alias = owner\n",
        "  drop owner\n",
        "  drop alias\n",
        "  let shared = Shared pointer\n",
        "  let raw: RawMutPtr<I32> = shared.*\n",
        "  printf (c_string \"%d %d\\n\", if (Gc.needs_drop.<Unique<I32>> ()) { 1 } else { 0 }, if (raw == pointer) { 1 } else { 0 })\n",
        "  ()\n",
        "}\n",
        "exercise ()\n",
    );
    assert_eq!(run(source), "0 1\n");
}
