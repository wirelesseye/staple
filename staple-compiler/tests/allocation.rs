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

fn lower(source: &str) -> LoweredModule {
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
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

#[cfg(unix)]
fn run(source: &str) -> String {
    let lowered = lower(source);
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
