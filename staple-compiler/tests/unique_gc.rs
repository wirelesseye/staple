//! Runs programs that force collections while `Unique` payloads are live,
//! raw, or part of a cycle through managed references.

// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
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

/// Builds and runs a program, returning its standard output.
#[cfg(unix)]
fn run(source: &str) -> String {
    let lowered = lower(source);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base =
        std::env::temp_dir().join(format!("staple-unique-gc-{}-{nonce}", std::process::id()));
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

/// Allocates well past the 1 MiB collection threshold.
const CHURN: &str = concat!(
    "def churn = () => () :: {\n",
    "    for index in (0 .. 200000) {\n",
    "        let garbage = Ref (index, index, index, index)\n",
    "        ()\n",
    "    }\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn collects_unique_payloads_in_cycles_through_managed_references() {
    // Ref Holder -> Unique Payload -> Ref (Option (Ref Holder)) -> Ref Holder.
    let source = format!(
        "{}{}",
        concat!(
            "use std.cinterop.*\n",
            "use std.ptr.*\n",
            "extern \"c\" {\n",
            "    printf: (RawPtr CChar, ...) -> I32\n",
            "}\n",
            "type Holder = wrap Unique Payload\n",
            "type Payload = wrap (tag: I32, back: Ref (Option (Ref Holder)))\n",
            "impl Drop Payload {\n",
            "    cleanup = Payload (tag, back) => {\n",
            "        printf (c_string \"dropped\\n\")\n",
            "        ()\n",
            "    }\n",
            "}\n",
            "def make_cycle = tag: I32 => () :: {\n",
            "    let empty: Option (Ref Holder) = None\n",
            "    let mut back: Ref (Option (Ref Holder)) = Ref empty\n",
            "    let holder = Ref (Holder (Unique (Payload (tag: tag, back: back))))\n",
            "    let previous = Ref.replace back (Some holder)\n",
            "    ()\n",
            "}\n",
            "def make_cycles = () => () :: {\n",
            "    for index in (0 .. 100) { make_cycle index }\n",
            "}\n",
        ),
        CHURN,
    ) + concat!("make_cycles ()\n", "churn ()\n", "churn ()\n");
    let output = run(&source);
    let dropped = output.lines().filter(|line| *line == "dropped").count();
    // A conservative stack scan may retain a stray cycle, but a payload that
    // is its own root retains every one of them.
    assert!(
        dropped >= 90,
        "only {dropped} of 100 cyclic Unique payloads were collected"
    );
}

#[cfg(unix)]
#[test]
fn unique_payloads_survive_collections_while_owned_or_raw() {
    let source = format!(
        "{}{}",
        concat!(
            "use std.cinterop.*\n",
            "use std.ptr.*\n",
            "extern \"c\" {\n",
            "    printf: (RawPtr CChar, ...) -> I32\n",
            "}\n",
        ),
        CHURN,
    ) + concat!(
        "def check = () => () :: {\n",
        "    let first_pair: (Ref I32, Ref I32) = (Ref 41, Ref 1)\n",
        "    let second_pair: (Ref I32, Ref I32) = (Ref 40, Ref 2)\n",
        "    let owned: Unique (Ref I32, Ref I32) = Unique first_pair\n",
        "    let lent: Unique (Ref I32, Ref I32) = Unique second_pair\n",
        "    let raw = Unique.unsafe_into_raw lent\n",
        "    churn ()\n",
        "    let restored: Unique (Ref I32, Ref I32) = Unique.unsafe_from_raw raw\n",
        "    churn ()\n",
        "    let (Ref first, Ref second) = owned.*\n",
        "    let (Ref third, Ref fourth) = restored.*\n",
        "    printf (c_string \"%d %d\\n\", first + second, third + fourth)\n",
        "    ()\n",
        "}\n",
        "check ()\n",
    );
    assert_eq!(run(&source), "42 42\n");
}

#[cfg(unix)]
#[test]
fn collects_list_storage_in_cycles_through_managed_references() {
    // Ref Holder -> List Payload storage -> Ref (Option closure) -> closure
    // environment -> Ref Holder. A closure closes the cycle because a type
    // cannot recurse through `List`.
    let source = format!(
        "{}{}",
        concat!(
            "use std.cinterop.*\n",
            "use std.ptr.*\n",
            "extern \"c\" {\n",
            "    printf: (RawPtr CChar, ...) -> I32\n",
            "}\n",
            "type Payload = wrap (tag: I32, back: Ref (Option (() -> I32)))\n",
            "type Holder = wrap (items: List Payload)\n",
            "impl Drop Payload {\n",
            "    cleanup = Payload (tag, back) => {\n",
            "        printf (c_string \"dropped\\n\")\n",
            "        ()\n",
            "    }\n",
            "}\n",
            "def make_cycle = tag: I32 => () :: {\n",
            "    let empty: Option (() -> I32) = None\n",
            "    let mut back: Ref (Option (() -> I32)) = Ref empty\n",
            "    let mut items: List Payload = List.new ()\n",
            "    List.push items (Payload (tag: tag, back: back))\n",
            "    let holder: Ref Holder = Ref (Holder (items: items))\n",
            "    let closure: () -> I32 = () => { let kept = holder; tag }\n",
            "    let previous = Ref.replace back (Some closure)\n",
            "    ()\n",
            "}\n",
            "def make_cycles = () => () :: {\n",
            "    for index in (0 .. 100) { make_cycle index }\n",
            "}\n",
        ),
        CHURN,
    ) + concat!("make_cycles ()\n", "churn ()\n", "churn ()\n");
    let output = run(&source);
    let dropped = output.lines().filter(|line| *line == "dropped").count();
    assert!(
        dropped >= 90,
        "only {dropped} of 100 cyclic List payloads were collected"
    );
}

/// Declares a `Tag` whose drop prints its identifier.
const TAG: &str = concat!(
    "use std.cinterop.*\n",
    "use std.ptr.*\n",
    "use std.buffer.Buffer\n",
    "use std.shared_buffer.SharedBuffer\n",
    "extern \"c\" {\n",
    "    printf: (RawPtr CChar, ...) -> I32\n",
    "}\n",
    "type Tag = wrap I32\n",
    "impl ! Copy Tag {}\n",
    "impl Drop Tag {\n",
    "    cleanup = Tag id => {\n",
    "        printf (c_string \"dropped %d\\n\", id)\n",
    "        ()\n",
    "    }\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn buffer_from_array_owns_and_drops_non_copy_elements_once() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "def scoped = () => () :: {\n",
            "    let buffer: Buffer Tag = Buffer.from_array (Tag 1, Tag 2, Tag 3)\n",
            "    printf (c_string \"length %d capacity %d\\n\", Buffer.length buffer, Buffer.capacity buffer)\n",
            "    ()\n",
            "}\n",
            "scoped ()\n",
            "printf (c_string \"done\\n\")\n",
        ),
    );
    let output = run(&source);
    assert_eq!(
        output,
        "length 3 capacity 3\ndropped 1\ndropped 2\ndropped 3\ndone\n"
    );
}

#[cfg(unix)]
#[test]
fn shared_buffer_set_drops_the_old_value_and_rejects_out_of_bounds() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "def run = () => () :: {\n",
            "    let mut buffer: SharedBuffer Tag = SharedBuffer.from_array (Tag 1, Tag 2)\n",
            "    match SharedBuffer.set buffer 0 (Tag 10) {\n",
            "        Some _ => printf (c_string \"replaced\\n\"),\n",
            "        None() => printf (c_string \"missing\\n\"),\n",
            "    }\n",
            "    match SharedBuffer.set buffer 2 (Tag 20) {\n",
            "        Some _ => printf (c_string \"replaced\\n\"),\n",
            "        None() => printf (c_string \"out of bounds\\n\"),\n",
            "    }\n",
            "    printf (c_string \"length %d\\n\", SharedBuffer.length buffer)\n",
            "    ()\n",
            "}\n",
            "run ()\n",
        ),
    );
    let output = run(&source);
    // The replaced element and the rejected value are each dropped
    // immediately; the buffer's remaining elements wait for collection.
    assert_eq!(
        output,
        "dropped 1\nreplaced\ndropped 20\nout of bounds\nlength 2\n"
    );
}

#[cfg(unix)]
#[test]
fn shared_buffer_get_and_set_share_writes_between_handles() {
    let source = concat!(
        "use std.cinterop.*\n",
        "use std.ptr.*\n",
        "use std.shared_buffer.SharedBuffer\n",
        "extern \"c\" {\n",
        "    printf: (RawPtr CChar, ...) -> I32\n",
        "}\n",
        "def run = () => () :: {\n",
        "    let mut first: SharedBuffer I32 = SharedBuffer.from_array (10, 20, 30)\n",
        "    let second = first\n",
        "    match SharedBuffer.get first 1 {\n",
        "        Some value => printf (c_string \"get %d\\n\", value),\n",
        "        None() => printf (c_string \"get missing\\n\"),\n",
        "    }\n",
        "    match SharedBuffer.get first 3 {\n",
        "        Some value => printf (c_string \"get %d\\n\", value),\n",
        "        None() => printf (c_string \"get out of bounds\\n\"),\n",
        "    }\n",
        "    match SharedBuffer.set first 1 21 {\n",
        "        Some _ => printf (c_string \"set\\n\"),\n",
        "        None() => printf (c_string \"set missing\\n\"),\n",
        "    }\n",
        "    match SharedBuffer.set first 3 99 {\n",
        "        Some _ => printf (c_string \"set\\n\"),\n",
        "        None() => printf (c_string \"set out of bounds\\n\"),\n",
        "    }\n",
        "    first[2] = 31\n",
        "    printf (c_string \"seen %d %d %d\\n\", second[0], second[1], second[2])\n",
        "    ()\n",
        "}\n",
        "run ()\n",
    );
    assert_eq!(
        run(source),
        "get 20\nget out of bounds\nset\nset out of bounds\nseen 10 21 31\n"
    );
}

#[cfg(unix)]
#[test]
fn shared_buffer_from_array_collects_non_copy_elements() {
    let source = format!(
        "{TAG}{}{}",
        concat!(
            "def make = tag: I32 => () :: {\n",
            "    let buffer: SharedBuffer Tag = SharedBuffer.from_array (Tag tag, Tag (tag + 1000))\n",
            "    ()\n",
            "}\n",
            "def make_all = () => () :: {\n",
            "    for index in (0 .. 100) { make index }\n",
            "}\n",
        ),
        CHURN,
    ) + concat!("make_all ()\n", "churn ()\n", "churn ()\n");
    let output = run(&source);
    let mut dropped: Vec<&str> = output
        .lines()
        .filter(|line| line.starts_with("dropped "))
        .collect();
    let total = dropped.len();
    dropped.sort_unstable();
    dropped.dedup();
    assert_eq!(
        dropped.len(),
        total,
        "an element was dropped more than once"
    );
    // A conservative stack scan may retain a stray buffer.
    assert!(
        total >= 180,
        "only {total} of 200 shared buffer elements were dropped"
    );
}

/// Allocates many short strings and capturing closures, reusing any freed
/// global storage with different contents.
const REFILL: &str = concat!(
    "def refill = () => () :: {\n",
    "    for index in (0 .. 200000) {\n",
    "        let text = \"zzzz\" + \"zzzz\"\n",
    "        let other = \"yyyy\" + \"yyyy\"\n",
    "        let closure = () => other\n",
    "        ()\n",
    "    }\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn module_globals_keep_strings_closures_and_arrays_alive() {
    let source = format!(
        "{}{}{}",
        concat!(
            "use std.cinterop.*\n",
            "use std.ptr.*\n",
            "extern \"c\" {\n",
            "    printf: (RawPtr CChar, ...) -> I32\n",
            "}\n",
            "def make_reader = text: String => (() -> String) :: () => text\n",
        ),
        CHURN,
        REFILL,
    ) + concat!(
        "let text = \"abcd\" + \"efgh\"\n",
        "let mut cell = \"a\"\n",
        "let set_cell = () => { cell = \"ijkl\" + \"mnop\" }\n",
        "let reader = make_reader (\"qrst\" + \"uvwx\")\n",
        "let pair: (Ref I32 ^ 2) = (Ref 40, Ref 2)\n",
        "set_cell ()\n",
        "churn ()\n",
        "refill ()\n",
        "churn ()\n",
        "refill ()\n",
        "let (Ref first, Ref second) = pair\n",
        "printf (c_string \"%d %d %d %d\\n\",\n",
        "    if (text == \"abcdefgh\") { 1 } else { 0 },\n",
        "    if (cell == \"ijklmnop\") { 1 } else { 0 },\n",
        "    if (reader () == \"qrstuvwx\") { 1 } else { 0 },\n",
        "    first + second)\n",
    );
    assert_eq!(run(&source), "1 1 1 42\n");
}

#[cfg(unix)]
#[test]
fn trees_recursing_through_buffers_drop_every_node_once() {
    let source = concat!(
        "use std.buffer.Buffer\n",
        "use std.shared_buffer.SharedBuffer\n",
        "use std.cinterop.*\n",
        "use std.ptr.*\n",
        "extern \"c\" {\n",
        "    printf: (RawPtr CChar, ...) -> I32\n",
        "}\n",
        "type Tag = wrap I32\n",
        "impl Drop Tag {\n",
        "    cleanup = Tag value => {\n",
        "        printf (c_string \"drop %d\\n\", value)\n",
        "        ()\n",
        "    }\n",
        "}\n",
        "type Owned = wrap (tag: Tag, children: Buffer Owned)\n",
        "type Shared = wrap (value: I32, children: SharedBuffer Shared)\n",
        "def leaf = value: I32 => Owned :: Owned (tag: Tag value, children: Buffer.with_capacity 0)\n",
        "def shared_total = tree: Shared => I32 :: {\n",
        "    let Shared (value, children) = tree\n",
        "    let mut sum = value\n",
        "    let mut index: USize = 0\n",
        "    while (index < SharedBuffer.length children) {\n",
        "        let Ref child = SharedBuffer.get_ref_unchecked children index\n",
        "        sum = sum + shared_total child\n",
        "        index = index + 1\n",
        "    }\n",
        "    sum\n",
        "}\n",
        "def run = () => () :: {\n",
        "    let mut middle: Buffer Owned = Buffer.with_capacity 2\n",
        "    Buffer.push middle (leaf 3)\n",
        "    Buffer.push middle (leaf 4)\n",
        "    let mut top: Buffer Owned = Buffer.with_capacity 2\n",
        "    Buffer.push top (leaf 2)\n",
        "    Buffer.push top (Owned (tag: Tag 5, children: middle))\n",
        "    let root = Owned (tag: Tag 1, children: top)\n",
        "    let empty: SharedBuffer Shared = SharedBuffer.from_array ()\n",
        "    let shared = Shared (value: 1, children: SharedBuffer.from_array (\n",
        "        Shared (value: 2, children: empty),\n",
        "        Shared (value: 3, children: empty),\n",
        "    ))\n",
        "    printf (c_string \"total %d\\n\", shared_total shared)\n",
        "    ()\n",
        "}\n",
        "run ()\n",
    );
    let output = run(source);
    let mut drops = output
        .lines()
        .filter_map(|line| line.strip_prefix("drop "))
        .collect::<Vec<_>>();
    drops.sort();
    assert_eq!(drops, ["1", "2", "3", "4", "5"], "{output}");
    assert!(output.contains("total 6\n"), "{output}");
}

#[cfg(unix)]
#[test]
fn trees_recursing_through_lists_drop_every_node_once() {
    let source = concat!(
        "use std.list.List\n",
        "use std.cinterop.*\n",
        "use std.ptr.*\n",
        "extern \"c\" {\n",
        "    printf: (RawPtr CChar, ...) -> I32\n",
        "}\n",
        "type Tag = wrap I32\n",
        "impl Drop Tag {\n",
        "    cleanup = Tag value => {\n",
        "        printf (c_string \"drop %d\\n\", value)\n",
        "        ()\n",
        "    }\n",
        "}\n",
        "type Node = wrap (tag: Tag, children: List Node)\n",
        "def leaf = value: I32 => Node :: Node (tag: Tag value, children: List.new ())\n",
        "def run = () => () :: {\n",
        "    let mut middle: List Node = List.new ()\n",
        "    List.push middle (leaf 3)\n",
        "    List.push middle (leaf 4)\n",
        "    let mut top: List Node = List.new ()\n",
        "    List.push top (leaf 2)\n",
        "    List.push top (Node (tag: Tag 5, children: middle))\n",
        "    let root = Node (tag: Tag 1, children: top)\n",
        "    ()\n",
        "}\n",
        "run ()\n",
    );
    let output = run(source);
    let mut drops = output
        .lines()
        .filter_map(|line| line.strip_prefix("drop "))
        .collect::<Vec<_>>();
    drops.sort();
    assert_eq!(drops, ["1", "2", "3", "4", "5"], "{output}");
}
