//! Shared code-generation corpus and strict emission/structure tests.

/// Where one corpus program's source comes from.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorpusSource {
    /// Source text inline in the corpus.
    Inline(&'static str),
    /// A path relative to the workspace root.
    File(&'static str),
}

/// What the CLI harness requires of one corpus program.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorpusExpectation {
    /// The program declares a C symbol no library defines (the census
    /// programs' `inspect`), so it cannot link or run. The CLI harness checks strict compilation.
    CompileOnly,
    /// The program must compile strictly and match the pinned behavior.
    MustRun,
}

/// The generic artifact family a `MustRun` fixture must instantiate twice.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorpusGenericArtifacts {
    /// A generic coroutine's `resume`/`cleanup` pair.
    CoroutinePairs,
    /// A generic reaction's runner.
    ReactionRunners,
    /// A generic `until`'s runner.
    UntilRunners,
    /// A generic derived binding's runner.
    DerivedRunners,
}

/// One code-generation corpus program.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct CorpusProgram {
    /// Stable label used in reports and failure messages.
    pub name: &'static str,
    pub source: CorpusSource,
    /// The feature family exercised by the entry.
    pub topic: &'static str,
    /// What the CLI harness requires of the program.
    pub expectation: CorpusExpectation,
    /// Template names from the program's own module
    /// (matched against `LoweredFunction::name`). Every instance of each
    /// listed template must be fully emitted by the lowered emitter and
    /// defined with its catalog name. This is the per-feature gate that does not
    /// need a runnable program.
    pub emits: &'static [&'static str],
    /// The exact stdout a `MustRun` program prints. The CLI harness asserts it
    /// under the default emitter.
    pub expected_stdout: Option<&'static str>,
    /// A `MustRun` program that must end in an `llvm.trap` (killed by a
    /// signal, so no exit code) under the default emitter.
    pub traps: bool,
    /// A `MustRun` program's generic artifact family: the in-process harness
    /// requires two distinct artifacts of it.
    pub generic_artifacts: Option<CorpusGenericArtifacts>,
}

const fn inline(name: &'static str, source: &'static str, topic: &'static str) -> CorpusProgram {
    CorpusProgram {
        name,
        source: CorpusSource::Inline(source),
        topic,
        expectation: CorpusExpectation::MustRun,
        emits: &[],
        expected_stdout: None,
        traps: false,
        generic_artifacts: None,
    }
}

const fn file(name: &'static str, path: &'static str, topic: &'static str) -> CorpusProgram {
    CorpusProgram {
        name,
        source: CorpusSource::File(path),
        topic,
        expectation: CorpusExpectation::MustRun,
        emits: &[],
        expected_stdout: None,
        traps: false,
        generic_artifacts: None,
    }
}

/// Requires distinct artifacts for multiple concrete generic instantiations.
const fn generic_artifact_fixture(
    mut program: CorpusProgram,
    generic_artifacts: CorpusGenericArtifacts,
) -> CorpusProgram {
    program.expectation = CorpusExpectation::MustRun;
    program.generic_artifacts = Some(generic_artifacts);
    program
}

/// Marks a corpus entry as unlinkable because its external symbol is undefined.
const fn compile_only(mut program: CorpusProgram) -> CorpusProgram {
    program.expectation = CorpusExpectation::CompileOnly;
    program
}

/// An entry that compiles strictly and runs identically under the default emitter.
const fn must_run(mut program: CorpusProgram) -> CorpusProgram {
    program.expectation = CorpusExpectation::MustRun;
    program
}

/// Requires a `MustRun` entry to end in a trap under the default emitter.
const fn expect_trap(mut program: CorpusProgram) -> CorpusProgram {
    program.traps = true;
    program
}

/// Pins the exact stdout a `MustRun` entry prints under the default emitter.
const fn expect_stdout(mut program: CorpusProgram, stdout: &'static str) -> CorpusProgram {
    program.expected_stdout = Some(stdout);
    program
}

/// Names the functions the entry must fully emit.
const fn emits(mut program: CorpusProgram, templates: &'static [&'static str]) -> CorpusProgram {
    program.emits = templates;
    program
}

/// The shared code-generation corpus: the empty program, a non-generic
/// integer-arithmetic program, a two-module program with module globals and
/// initialization state, the artifact census programs plus an
/// every-artifact-family fixture, `staple-compiler/examples/*.sta` (excluding
/// `macros.sta`, covered by the standalone example gate), and `game_loop`
/// example.
#[doc(hidden)]
pub fn codegen_corpus() -> &'static [CorpusProgram] {
    &CORPUS
}

static CORPUS: [CorpusProgram; 89] = [
    expect_stdout(
        must_run(inline(
            "from_types",
            concat!(
                "use std.io.println\n",
                "type Meters = from F64\n",
                "type Label = from String\n",
                "type Triple = from (I32, I32, I32)\n",
                "type Other\n",
                "def show: Meters -> F64 = meters => meters.*\n",
                "def kind: Meters | Other -> String = value => match value {\n",
                "    meters: Meters => \"meters ${meters.*}\",\n",
                "    _: Other => \"other\",\n",
                "}\n",
                "def describe: Bool | I32 -> String = value => match value {\n",
                "    True => \"yes\",\n",
                "    False => \"no\",\n",
                "    number: I32 => \"${number}\",\n",
                "}\n",
                "def bump: Option I32 -> Option I32 = value => {\n",
                "    let Some(number)? = value\n",
                "    Some (number + 1)\n",
                "}\n",
                "def unwrap: Option I32 -> I32 = value => match value {\n",
                "    Some number => number,\n",
                "    None => 0,\n",
                "}\n",
                "def run = () => {\n",
                "    let distance = 2.5\n",
                "    let label: Label = \"route\"\n",
                "    let mut triple: Triple = (1, 2, 3)\n",
                "    triple[1] = 20\n",
                "    println \"${show 1.5} ${show distance} ${label.*}\"\n",
                "    println \"${kind 4.0} ${describe True} ${describe 7}\"\n",
                "    println \"${unwrap (bump (Some 4))} ${unwrap (bump None)} ${triple[1]}\"\n",
                "}\n",
                "run ()\n",
            ),
            "types",
        )),
        "1.5 2.5 route\nmeters 4 yes 7\n5 0 20\n",
    ),
    expect_stdout(
        must_run(inline(
            "parameter_products",
            concat!(
                "use std.io.println\n",
                "type Inputs = alias [I32, I32]\n",
                "type Callable Args Result = alias Args -> Result\n",
                "type Add = alias Callable Inputs I32\n",
                "type Handler Args = wrap (callback: Args -> I32, label: String)\n",
                "def add: Add = [a, b] => a + b\n",
                "def keep: <A> [Handler A, A -> I32] -> Handler A = [handler, callback] => Handler (callback: callback, label: handler.label)\n",
                "def forward: <A> (A -> I32) -> A -> I32 = f => f\n",
                "let handler: Handler Inputs = Handler (callback: add, label: \"sum\")\n",
                "let kept = keep handler add\n",
                "let callback: Inputs -> I32 = forward add\n",
                "println \"${add 1 2}\"\n",
                "println \"${kept.callback 3 4}\"\n",
                "println \"${callback 5 6}\"\n",
            ),
            "parameter-products",
        )),
        "3\n7\n11\n",
    ),
    expect_stdout(must_run(inline("empty", "", "core")), ""),
    expect_stdout(
        must_run(inline(
            "integer_arithmetic",
            concat!(
                "def plus: (I32, I32) -> I32 = (left, right) => left + right\n",
                "let first = plus (1, 2)\n",
                "let second = first + 3\n",
            ),
            "core",
        )),
        "",
    ),
    expect_stdout(
        must_run(inline(
            "module_globals",
            concat!(
                "mod first {\n",
                "    pub let base = 1\n",
                "    pub let doubled = base + base\n",
                "}\n",
                "mod second {\n",
                "    pub let extra = 3\n",
                "    pub let total = extra + extra\n",
                "}\n",
                "let answer = first.doubled + second.total\n",
            ),
            "core",
        )),
        "",
    ),
    expect_stdout(
        must_run(inline(
            "census_constructor_adapters",
            concat!(
                "type Point = wrap (I32, I32)\n",
                "let make: () -> ((I32, I32) -> Point) = () => Point\n",
                "type Resource = wrap I32\n",
                "impl Drop Resource { drop = Resource value => () }\n",
                "let make_resource: () -> (Resource -> Ref Resource) = () => Ref\n",
                "def ref_maker: <T where Copy T> () -> (T -> Ref T) = () => Ref\n",
                "let maker_i32: I32 -> Ref I32 = ref_maker ()\n",
                "let maker_u8: U8 -> Ref U8 = ref_maker ()\n",
            ),
            "core",
        )),
        "",
    ),
    expect_stdout(
        must_run(inline(
            "census_structural_methods",
            concat!(
                "def show_pair: (I32, I32) -> String = pair => \"${pair:?}\"\n",
                "def pick: Bool -> (I32 | U8) = condition => when { condition => 1, else => (1 satisfies U8) }\n",
                "def show_sum: (I32 | U8) -> String = value => \"${value:?}\"\n",
                "def index_mixed: (U8, I32) -> (I32 | U8) = pair => pair[0]\n",
                "def count_pair: (U8, I32) -> I32 = pair => {\n",
                "  let mut count = 0\n",
                "  for item in pair { count = count + 1 }\n",
                "  count\n",
                "}\n",
                "def deref_mixed: (Ref (U8, I32)) -> (I32 | U8) = reference => reference[0]\n",
                "let a = show_pair (1, 2)\n",
                "let b = show_sum (pick True)\n",
                "let c = index_mixed ((1 satisfies U8), 2)\n",
                "let d = count_pair ((1 satisfies U8), 2)\n",
                "let e = deref_mixed (Ref ((1 satisfies U8), 2))\n",
            ),
            "core",
        )),
        "",
    ),
    compile_only(inline(
        "census_cleanup",
        concat!(
            "use std.cinterop.(CString, c_string)\n",
            "use std.buffer.*\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "extern \"c\" { abs: I32 -> I32 }\n",
            "def capture: move CString -> (() -> I32) = move value => () => inspect value\n",
            "def counter: () -> I32 = () => {\n",
            "  let mut total = 0\n",
            "  let bump = () => { total = total + 1 }\n",
            "  total\n",
            "}\n",
            "let mut strings: Buffer CString = Buffer.with_capacity (2 satisfies USize)\n",
            "let run = capture (c_string \"x\")\n",
            "let absolute = abs\n",
            "let counted = counter ()\n",
        ),
        "core",
    )),
    compile_only(inline(
        "census_coroutines_and_runners",
        concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "let signal flag = 0\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def owning: move CString -> Coroutine{} I32 = move value => coro { inspect value; 1 }\n",
            "def peek: <T> [T] -> I32 = _ => 1\n",
            "def generic: <T where Copy T> T -> Coroutine{} I32 = value => coro { peek value; 1 }\n",
            "def waiting: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { flag >= 1 })\n",
            "  ()\n",
            "}\n",
            "let a = task ()\n",
            "let b = owning (c_string \"x\")\n",
            "let c: Coroutine{} I32 = generic 1\n",
            "let d: Coroutine{} I32 = generic (1 satisfies U8)\n",
            "let e = with Reactive = reactive_scope () { waiting () }\n",
            "let f = with Reactive = reactive_scope () { reaction { () } }\n",
            "let doubled = flag + flag\n",
        ),
        "core",
    )),
    compile_only(inline(
        "all_artifact_families",
        concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "use std.buffer.*\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "extern \"c\" { abs: I32 -> I32 }\n",
            "type Point = wrap (I32, I32)\n",
            "let make_point: () -> ((I32, I32) -> Point) = () => Point\n",
            "let pair = (1, 2)\n",
            "let shown = \"${pair:?}\"\n",
            "type Resource = wrap I32\n",
            "impl Drop Resource { drop = Resource value => () }\n",
            "let reference: Ref Resource = Ref (Resource 1)\n",
            "def capture: move CString -> (() -> I32) = move value => () => inspect value\n",
            "def counter: () -> I32 = () => {\n",
            "  let mut total = 0\n",
            "  let bump = () => { total = total + 1 }\n",
            "  total\n",
            "}\n",
            "let mut strings: Buffer CString = Buffer.with_capacity (1 satisfies USize)\n",
            "let run = capture (c_string \"x\")\n",
            "let counted = counter ()\n",
            "let absolute = abs\n",
            "let signal flag = 0\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def waiting: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { flag >= 1 })\n",
            "  ()\n",
            "}\n",
            "let started = task ()\n",
            "let reaction_scope = with Reactive = reactive_scope () { reaction { () } }\n",
            "let coroutine_scope = with Reactive = reactive_scope () { waiting () }\n",
            "let doubled = flag + flag\n",
        ),
        "core",
    )),
    expect_stdout(
        must_run(file(
            "example_c_interop",
            "staple-compiler/examples/c_interop.sta",
            "core",
        )),
        "hello, world!\nowned Staple String\nCString round trip\n",
    ),
    expect_stdout(
        must_run(file(
            "example_coroutines",
            "staple-compiler/examples/coroutines.sta",
            "core",
        )),
        "-- pump 1 --\nworker 4: start\nworker 9: start\npump 1: executed=4 ready=2\n-- pump 2 --\nworker 4: done\ngreeter: host sent 200\npump 2: executed=4 ready=1\n-- pump 3 --\nconsumer: worker produced 40\npump 3: executed=1 ready=0\ndoomed finished: True\nscope closed\n",
    ),
    expect_stdout(
        must_run(file(
            "example_hello_world",
            "staple-compiler/examples/hello_world.sta",
            "core",
        )),
        "Hello, world!\n",
    ),
    expect_stdout(
        must_run(file(
            "example_language_tour",
            "staple-compiler/examples/language_tour.sta",
            "core",
        )),
        "Staple language tour\nProducts, closures, nested patterns, and operators evaluated successfully.\n",
    ),
    expect_stdout(
        must_run(file(
            "example_list_and_collections",
            "staple-compiler/examples/list_and_collections.sta",
            "core",
        )),
        "8\n8\n0\n7\n128\n7\n7\n3\n",
    ),
    expect_stdout(
        must_run(file(
            "example_modules_and_imports",
            "staple-compiler/examples/modules_and_imports.sta",
            "core",
        )),
        "module imports\nNamespace, selected, renamed, and glob imports evaluated successfully.\n",
    ),
    expect_stdout(
        must_run(file(
            "example_signals_and_reactions",
            "staple-compiler/examples/signals_and_reactions.sta",
            "core",
        )),
        "count: 0, doubled: 0\ncount: 3, doubled: 6\ninitial snapshot: 0, current count: 3\n",
    ),
    expect_stdout(
        must_run(file(
            "example_sums_and_propagation",
            "staple-compiler/examples/sums_and_propagation.sta",
            "core",
        )),
        "found\nnot found\nExplicit return evaluated successfully.\n",
    ),
    expect_stdout(
        must_run(file(
            "example_traits_and_generics",
            "staple-compiler/examples/traits_and_generics.sta",
            "core",
        )),
        "integer\ntrue\n",
    ),
    expect_stdout(
        must_run(file(
            "example_types_and_matching",
            "staple-compiler/examples/types_and_matching.sta",
            "core",
        )),
        "Nominal patterns, generic constructors, and singleton matches evaluated successfully.\ndifferent\n",
    ),
    // A two-module program: `main.sta` resolves `use game.*` against its own
    // directory.
    expect_stdout(
        must_run(file(
            "example_game_loop",
            "staple-compiler/examples/game_loop/main.sta",
            "core",
        )),
        "fixed tick 1\nfixed tick 2\nfixed tick 3\nentity destroyed\nwall sleeper woke: wall_ms=300\nfinal: game_ms=100 wall_ms=400 frame=4 fixed=3\n",
    ),
    expect_stdout(
        must_run(
            // Calls, callable values, closures, resources, and intrinsics. Each
            // entry names the functions its `emits` list must fully emit.
            emits(
                inline(
                    "calls_generic",
                    concat!(
                        "def identity: <T> move T -> T = move value => value\n",
                        "def first: <T where Copy T> T -> T = value => value\n",
                        "let one = identity 41\n",
                        "let text = identity \"hello\"\n",
                        "let two = first 2\n",
                        "let total = one + two\n",
                    ),
                    "calls",
                ),
                &["identity", "first"],
            ),
        ),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "calls_curried_defaults",
                concat!(
                    "type Sum3 = from (I32, b: I32 = 2, c: I32 = 3)\n",
                    "def sum3: Sum3 -> I32 = args => {\n",
                    "    let (a, b, c) = args\n",
                    "    a + b + c\n",
                    "}\n",
                    "def pair_of: (I32, I32) -> (I32, I32) = (left, right) => (left, right)\n",
                    "def total: () -> I32 = () => sum3 (1, .c: 9)\n",
                    "def spread: (I32, I32) -> I32 = (left, right) => {\n",
                    "    let pair = pair_of (left, right)\n",
                    "    sum3 (1, ...pair)\n",
                    "}\n",
                    "def curried: I32 -> I32 -> I32 = a => b => a + b\n",
                    "let value = total ()\n",
                    "let spread_value = spread (4, 5)\n",
                    "let make = curried 1\n",
                    "let curried_value = make 2\n",
                ),
                "calls",
            ),
            &["total", "spread", "curried", "sum3", "pair_of"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "calls_mutation",
                concat!(
                    "type MoveOnly = wrap I32\n",
                    "impl !Copy MoveOnly {}\n",
                    "def bump: mut I32 -> () = mut target => { target = target + 1 }\n",
                    "def borrow: MoveOnly -> I32 = value => 1\n",
                    "def exercise: I32 -> I32 = seed => {\n",
                    "    let mut total = seed\n",
                    "    bump total\n",
                    "    bump (seed + 1)\n",
                    "    let value = MoveOnly 2\n",
                    "    total + borrow value + borrow (MoveOnly 3)\n",
                    "}\n",
                    "let result = exercise 1\n",
                ),
                "calls",
            ),
            &["bump", "borrow", "exercise"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "closures_captures",
                concat!(
                    "type MoveOnly = wrap I32\n",
                    "impl !Copy MoveOnly {}\n",
                    // A `move T` parameter cannot be moved into a nested closure
                    // (the checker rejects moving out of the borrowed parameter
                    // storage), so the value-capture case uses a `Copy` bound; the
                    // move-into-closure shape is covered by `extern_values` and
                    // `constructors`.
                    "def keeper: <T where Copy T> T -> (() -> T) = value => () => value\n",
                    "def counter: <T where Copy T> T -> (() -> T) = start => {\n",
                    "    let mut total = start\n",
                    "    let current = () => total\n",
                    "    current\n",
                    "}\n",
                    "def reader: <T> (() -> T) -> T = callback => callback ()\n",
                    "def consume: MoveOnly -> I32 = value => 1\n",
                    "def borrowed: MoveOnly -> I32 = value => {\n",
                    "    let peek = () => consume value\n",
                    "    peek ()\n",
                    "}\n",
                    "let held = keeper 7\n",
                    "let counted = counter 0\n",
                    "let first = reader held\n",
                    "let second = reader counted\n",
                    "let third = borrowed (MoveOnly 4)\n",
                ),
                "calls",
            ),
            &["keeper", "counter", "reader", "borrowed", "consume"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "extern_values",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "extern \"c\" {\n",
                    "    puts: CString -> I32\n",
                    "}\n",
                    "def call_extern: CString -> I32 = value => puts value\n",
                    // An extern used as a closure value inside a function (the
                    // `ExternAdapterValue` adapter), not only in an initializer.
                    "def extern_value: () -> (CString -> I32) = () => puts\n",
                    "let adapter = extern_value ()\n",
                    "let owned = c_string \"staple\\n\"\n",
                    "let first = call_extern owned\n",
                    "let second = puts (c_string \"again\\n\")\n",
                    "let as_value = puts\n",
                ),
                "calls",
            ),
            &["call_extern", "extern_value"],
        )),
        "staple\n\nagain\n\n",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "constructors",
                concat!(
                    "type Point = wrap (I32, I32)\n",
                    "type Resource = wrap I32\n",
                    "impl Drop Resource { drop = Resource value => () }\n",
                    "def make: I32 -> Point = x => Point (x, x)\n",
                    "def call_make: I32 -> Point = x => make x\n",
                    // A managed `Ref` construction inside a function, so the
                    // payload-finalizer path is a focus body, not only an
                    // initializer.
                    "def make_ref: I32 -> Ref Resource = x => Ref (Resource x)\n",
                    "let made = make_ref 5\n",
                    "let point = Point (1, 2)\n",
                    "let reference: Ref Resource = Ref (Resource 1)\n",
                    "let make_point: () -> ((I32, I32) -> Point) = () => Point\n",
                    "let point_value = make_point ()\n",
                    "let second = point_value (3, 4)\n",
                ),
                "calls",
            ),
            &["make", "call_make", "make_ref"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "resources_with",
                concat!(
                    "pub type Counter = pub wrap (value: I32)\n",
                    "def get: () ->{Counter} I32 = () => 1\n",
                    "def forward: () ->{Counter} I32 = () => get ()\n",
                    "def value_of: Counter -> I32 = counter => counter.value\n",
                    "def read: () ->{Counter} I32 = () => value_of (resource Counter)\n",
                    // A resource assignment place through a `with mut` provider.
                    "def bump: () ->{mut Counter} () = () => {\n",
                    "    (resource Counter).value = 1\n",
                    "}\n",
                    "def run: I32 -> I32 = seed => {\n",
                    "    let counter = Counter (value: seed)\n",
                    "    with Counter = counter { forward () + read () }\n",
                    "}\n",
                    "let mut shared = Counter (value: 0)\n",
                    "with mut Counter = shared { bump () }\n",
                ),
                "calls",
            ),
            &["get", "forward", "value_of", "read", "run", "bump"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "numeric_intrinsics",
                concat!(
                    "use std.slice.Slice\n",
                    "use std.string.ToString\n",
                    "def integers: (I32, I32) -> I32 = (left, right) => left + right * left - right / left\n",
                    "def compare: (I32, I32) -> Bool = (left, right) => left < right\n",
                    "def floats: (F64, F64) -> F64 = (left, right) => left * right + left / right\n",
                    "def float_compare: (F32, F32) -> Bool = (left, right) => left >= right\n",
                    "def describe: I32 -> String = value => ToString.to_string value\n",
                    "def combine: (String, String) -> String = (left, right) => left + right\n",
                    "def bytes_length: String -> USize = text => Slice.length (String.bytes text)\n",
                    "let first = describe 41\n",
                    "let second = combine (\"staple\", \"!\")\n",
                    "let third = bytes_length second\n",
                ),
                "calls",
            ),
            &[
                "integers",
                "compare",
                "floats",
                "float_compare",
                "describe",
                "combine",
                "bytes_length",
            ],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "thunk_arguments",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "def evaluate: (() -> I32) -> I32 = callback => callback ()\n",
                    "def thunk_plain: I32 -> I32 = value => evaluate { value + 1 }\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    // The thunk captures an owned `CString`, so lowering records a
                    // `ThunkArgumentEnvironment` finalizer use. `thunk_env` owns
                    // its moved parameter; emission emits its scope exit.
                    // emission fixed the extern adapter ABI, so the thunk
                    // can now call the `puts` extern value and print the text.
                    "def thunk_env: move CString -> I32 = move value => evaluate { puts value }\n",
                    "let first = thunk_plain 1\n",
                    "let second = thunk_env (c_string \"thunk\\n\")\n",
                ),
                "calls",
            ),
            &["evaluate", "thunk_plain", "thunk_env"],
        )),
        "thunk\n\n",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "match_sums_products",
                concat!(
                    "type Ok T = wrap T\n",
                    "type IOError = wrap String\n",
                    "type Inner = wrap (I32, I32)\n",
                    "type Sign = wrap I32\n",
                    "\n",
                    "def nested: Ok (Inner | IOError) | IOError -> I32 = result => match result {\n",
                    "    Ok inner => match inner {\n",
                    "        Inner pair@(left, right) => left + right,\n",
                    "        IOError _ => 0,\n",
                    "    },\n",
                    "    IOError _ => 0,\n",
                    "}\n",
                    "\n",
                    "def nominal: Sign | Bool -> I32 = value => match value {\n",
                    "    Sign inner => inner,\n",
                    "    True => 1,\n",
                    "    False => 0,\n",
                    "}\n",
                    "\n",
                    "def wildcard: Bool -> I32 = value => match value {\n",
                    "    True => 1,\n",
                    "    _ => 0,\n",
                    "}\n",
                    "\n",
                    "def flag: Bool -> I32 = value => match value {\n",
                    "    True => 1,\n",
                    "    False => 0,\n",
                    "}\n",
                    "\n",
                    "let inner: Inner | IOError = Inner (1, 2)\n",
                    "let result: Ok (Inner | IOError) | IOError = Ok inner\n",
                    "let first = nested result\n",
                    "let second = nominal (Sign 7)\n",
                    "let third = wildcard False\n",
                    "let fourth = flag True\n",
                ),
                "expressions",
            ),
            &["nested", "nominal", "wildcard", "flag"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "match_strings_literals",
                concat!(
                    "def fallback: String -> I32 = value => match value {\n",
                    "    \"literal\" => 1,\n",
                    "    text => 2,\n",
                    "}\n",
                    "\n",
                    "def describe: \"literal\" -> I32 = value => match value {\n",
                    "    \"literal\" => 1,\n",
                    "}\n",
                    "\n",
                    "let first = fallback \"literal\"\n",
                    "let second = describe \"literal\"\n",
                ),
                "expressions",
            ),
            &["fallback", "describe"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "destructuring",
                concat!(
                    "pub type Pair = pub wrap (I32, I32)\n",
                    "type Outer = wrap (Pair, String)\n",
                    "\n",
                    "def sum_pair: Pair -> I32 = Pair (left, right) => left + right\n",
                    "def destructure: (Pair, I32) -> I32 = (Pair (left, right), extra) => left + right + extra\n",
                    "def nested = value: Outer => {\n",
                    "    let Outer (Pair (left, right), text) = value\n",
                    "    left + right\n",
                    "}\n",
                    "\n",
                    "let pair = Pair (1, 2)\n",
                    "let first = sum_pair pair\n",
                    "let second = destructure (pair, 3)\n",
                    "let outer = Outer (pair, \"text\")\n",
                    "let third = nested outer\n",
                ),
                "expressions",
            ),
            &["sum_pair", "destructure", "nested"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "places_assignment",
                concat!(
                    "type Counter = wrap I32\n",
                    "type Wrapper = wrap (value: I32)\n",
                    "impl Index Counter String I32 { index = (counter, key) => 0 }\n",
                    "impl MutateIndex Counter String I32 { mutate_index = (mut counter, key, move value) => () }\n",
                    "def make_counter = () => Counter 0\n",
                    "\n",
                    "def places = (mut direct: I32, mut pair: (I32, I32), mut wrapper: Wrapper, mut counter: Counter, mut values: (I32; 2)) => {\n",
                    "    direct = 1\n",
                    "    pair.0 = 2\n",
                    "    wrapper.value = 3\n",
                    "    counter.* = 4\n",
                    "    values[0] = 5\n",
                    "    (make_counter())[\"key\"] = 6\n",
                    "}\n",
                    "\n",
                    "def ref_place: mut Ref (I32, I32) -> () = mut reference => {\n",
                    "    reference.0 = 1\n",
                    "}\n",
                    "\n",
                    "let mut direct: I32 = 0\n",
                    "let mut pair: (I32, I32) = (0, 0)\n",
                    "let mut wrapper: Wrapper = Wrapper (value: 0)\n",
                    "let mut counter: Counter = Counter 0\n",
                    "let mut values: (I32; 2) = (0, 0)\n",
                    "places (direct, pair, wrapper, counter, values)\n",
                ),
                "expressions",
            ),
            &["places", "ref_place", "make_counter"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "coercions",
                concat!(
                    "use std.slice.Slice\n",
                    "\n",
                    "type Ok T = wrap T\n",
                    "type IOError = wrap String\n",
                    "type Other = wrap String\n",
                    "\n",
                    "def read: () -> Ok I32 | IOError = () => Ok (42)\n",
                    "def widen: () -> Ok I32 | IOError | Other = () => read ()\n",
                    "def inject: Ok I32 -> Ok I32 | IOError = value => value\n",
                    "def slice_ref: Ref (I32; 2) -> Slice I32 = value => value\n",
                    "def take: Ok I32 | IOError -> I32 = value => 1\n",
                    "\n",
                    "def consume: () -> I32 = () => {\n",
                    "    let injected = inject (Ok (1))\n",
                    "    let widened = widen ()\n",
                    "    take injected\n",
                    "}\n",
                    "\n",
                    "let source: (I32; 2) = (1, 2)\n",
                    "let sliced = slice_ref (Ref source)\n",
                    "let used = consume ()\n",
                ),
                "expressions",
            ),
            &["read", "widen", "inject", "slice_ref", "take", "consume"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "loops_values",
                concat!(
                    "def sum_to: I32 -> I32 = limit => {\n",
                    "    let mut total: I32 = 0\n",
                    "    let mut index: I32 = 0\n",
                    "    loop {\n",
                    "        if (index > limit) { break total }\n",
                    "        total = total + index\n",
                    "        index = index + 1\n",
                    "        continue\n",
                    "    }\n",
                    "}\n",
                    "\n",
                    "def nested: () -> I32 = () => {\n",
                    "    let mut outer: I32 = 0\n",
                    "    loop {\n",
                    "        let mut inner: I32 = 0\n",
                    "        loop {\n",
                    "            inner = inner + 1\n",
                    "            if (inner == 2) { break }\n",
                    "        }\n",
                    "        outer = outer + inner\n",
                    "        if (outer > 3) { break outer }\n",
                    "    }\n",
                    "}\n",
                    "\n",
                    "let first = sum_to 4\n",
                    "let second = nested ()\n",
                ),
                "expressions",
            ),
            &["sum_to", "nested"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "propagation",
                concat!(
                    "type Found T = wrap T\n",
                    "type Missing = wrap String\n",
                    "\n",
                    "def lookup: Bool -> Found I32 | Missing = available => if available { Found (42) } else { Missing \"absent\" }\n",
                    "\n",
                    "def doubled = available: Bool => {\n",
                    "    let Found(value)? = lookup available\n",
                    "    Found (value + value)\n",
                    "}\n",
                    "\n",
                    "let first = doubled True\n",
                    "let second = doubled False\n",
                ),
                "expressions",
            ),
            &["lookup", "doubled"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "templates",
                concat!(
                    "def show: I32 -> String = value => \"display ${value}\"\n",
                    "def debug: I32 -> String = value => \"debug ${value:?}\"\n",
                    "def both: I32 -> String = value => \"a=${value} b=${value:?}\"\n",
                    "\n",
                    "let first = show 1\n",
                    "let second = debug 2\n",
                    "let third = both 3\n",
                ),
                "expressions",
            ),
            &["show", "debug", "both"],
        )),
        "",
    ),
    // Ownership cleanup, finalizers, and buffers.
    must_run(expect_stdout(
        emits(
            inline(
                "drop_order",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Tag = wrap CString\n",
                    "impl Drop Tag { drop = Tag text => { puts text; () } }\n",
                    "\n",
                    "def scoped: () -> () = () => {\n",
                    "    let a = Tag (c_string \"scope\")\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "def early: () -> I32 = () => {\n",
                    "    let b = Tag (c_string \"early\")\n",
                    "    return 1\n",
                    "}\n",
                    "\n",
                    "type Ok = wrap I32\n",
                    "type Bad = wrap CString\n",
                    "impl Drop Bad { drop = Bad text => { puts text; () } }\n",
                    "def fails: () -> Ok | Bad = () => Bad (c_string \"failure\")\n",
                    "def propagated: () -> Ok | Bad = () => {\n",
                    "    let c = Tag (c_string \"propagate\")\n",
                    "    let Ok(value)? = fails ()\n",
                    "    Ok value\n",
                    "}\n",
                    "\n",
                    "def looped: () -> I32 = () => {\n",
                    "    let mut index = 0\n",
                    "    loop {\n",
                    "        let d = Tag (c_string \"loop\")\n",
                    "        if (index == 1) { break 0 }\n",
                    "        let e = Tag (c_string \"continue\")\n",
                    "        index = index + 1\n",
                    "        continue\n",
                    "    }\n",
                    "}\n",
                    "\n",
                    "def matched: Bool -> I32 = condition => {\n",
                    "    let f = Tag (c_string \"match\")\n",
                    "    match condition {\n",
                    "        True => { let arm = Tag (c_string \"match arm\"); 1 },\n",
                    "        False => 0,\n",
                    "    }\n",
                    "}\n",
                    "\n",
                    "def logical: Bool -> Bool = flag => flag && { let right = Tag (c_string \"logical\"); True }\n",
                    "\n",
                    "def moved: () -> () = () => {\n",
                    "    let g = Tag (c_string \"moved\")\n",
                    "    let h = g\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "def replaced: () -> () = () => {\n",
                    "    let mut i = Tag (c_string \"replace old\")\n",
                    "    i = Tag (c_string \"replace new\")\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "def discarded: () -> () = () => {\n",
                    "    Tag (c_string \"discard\")\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "def bump: mut Tag -> () = mut target => ()\n",
                    "def called: () -> () = () => {\n",
                    "    bump (Tag (c_string \"temporary\"))\n",
                    "}\n",
                    "\n",
                    "scoped ()\n",
                    "early ()\n",
                    "propagated ()\n",
                    "looped ()\n",
                    "matched True\n",
                    "logical True\n",
                    "moved ()\n",
                    "replaced ()\n",
                    "discarded ()\n",
                    "called ()\n",
                ),
                "ownership",
            ),
            &[
                "scoped",
                "early",
                "propagated",
                "fails",
                "looped",
                "matched",
                "moved",
                "replaced",
                "discarded",
                "called",
                "bump",
                "logical",
            ],
        ),
        "scope\nearly\npropagate\nfailure\ncontinue\nloop\nloop\nmatch arm\nmatch\nlogical\nmoved\nreplace old\nreplace new\ndiscard\ntemporary\n",
    )),
    expect_stdout(
        must_run(emits(
            inline(
                "drop_glue_shapes",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Handle = wrap CString\n",
                    "impl Drop Handle { drop = Handle text => { puts text; () } }\n",
                    "type Wrapper = wrap Handle\n",
                    "type Left = wrap (CString, I32)\n",
                    "type Right = wrap (I32, I32)\n",
                    "type Empty = wrap ()\n",
                    "type Chain = wrap (I32, Ref (Empty | Chain))\n",
                    "\n",
                    "def nested_product: () -> () = () => {\n",
                    "    let value: ((CString, I32), (I32, CString)) = ((c_string \"nested left\\n\", 1), (2, c_string \"nested right\\n\"))\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "def sum_choice: Bool -> (Left | Right) = condition => when {\n",
                    "    condition => Left (c_string \"sum left\\n\", 1),\n",
                    "    else => Right (1, 2),\n",
                    "}\n",
                    "\n",
                    "def wrapped: () -> () = () => {\n",
                    "    let value: Wrapper = Wrapper (Handle (c_string \"wrapped\\n\"))\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "def recursive: () -> () = () => {\n",
                    "    let value: Chain = Chain (1, Ref ((Empty ()) satisfies (Empty | Chain)))\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "nested_product ()\n",
                    "sum_choice True\n",
                    "sum_choice False\n",
                    "wrapped ()\n",
                    "recursive ()\n",
                ),
                "ownership",
            ),
            &["nested_product", "sum_choice", "wrapped", "recursive"],
        )),
        "wrapped\n\n",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "finalizers",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Payload = wrap CString\n",
                    "impl Drop Payload { drop = Payload value => () }\n",
                    "\n",
                    "def make_ref: () -> Ref Payload = () => Ref (Payload (c_string \"ref\"))\n",
                    "\n",
                    "def cell_finalizer: () -> (() -> I32) = () => {\n",
                    "    let mut cell = c_string \"cell\"\n",
                    "    cell = c_string \"cell again\"\n",
                    "    () => puts cell\n",
                    "}\n",
                    "\n",
                    "def closure_environment: move CString -> (() -> I32) = move value => () => puts value\n",
                    "\n",
                    "let reference = make_ref ()\n",
                    "let celled = cell_finalizer ()\n",
                    "let closure = closure_environment (c_string \"environment\")\n",
                ),
                "ownership",
            ),
            &["make_ref", "cell_finalizer", "closure_environment"],
        )),
        "",
    ),
    expect_stdout(
        expect_trap(must_run(emits(
            inline(
                "buffers",
                concat!(
                    "use std.buffer.Buffer\n",
                    "use std.slice.Slice\n",
                    "use std.cinterop.(CString, c_string)\n",
                    "type Tag = wrap CString\n",
                    "impl Drop Tag { drop = Tag text => () }\n",
                    "impl Clone Tag { clone = Tag text => Tag (c_string \"clone\") }\n",
                    "\n",
                    "def basics: () -> USize = () => {\n",
                    "    let mut values: Buffer I32 = Buffer.with_capacity (2 satisfies USize)\n",
                    "    Buffer.push values 1\n",
                    "    Buffer.push values 2\n",
                    "    let length: USize = Buffer.length values\n",
                    "    let capacity: USize = Buffer.capacity values\n",
                    "    let first: Ref I32 = Buffer.get_ref values (0 satisfies USize)\n",
                    "    let popped: Option I32 = Buffer.pop values\n",
                    "    length + capacity\n",
                    "}\n",
                    "\n",
                    "def transfer: () -> USize = () => {\n",
                    "    let mut source: Buffer I32 = Buffer.with_capacity (2 satisfies USize)\n",
                    "    Buffer.push source 1\n",
                    "    Buffer.push source 2\n",
                    "    let mut destination: Buffer I32 = Buffer.with_capacity (4 satisfies USize)\n",
                    "    Buffer.transfer source destination\n",
                    "    Buffer.length destination\n",
                    "}\n",
                    "\n",
                    "def clone_tags: () -> USize = () => {\n",
                    "    let mut tags: Buffer Tag = Buffer.with_capacity (2 satisfies USize)\n",
                    "    Buffer.push tags (Tag (c_string \"tag one\\n\"))\n",
                    "    let cloned: Buffer Tag = Clone.clone tags\n",
                    "    Buffer.length cloned\n",
                    "}\n",
                    "\n",
                    "def freeze: () -> USize = () => {\n",
                    "    let mut values: Buffer I32 = Buffer.with_capacity (2 satisfies USize)\n",
                    "    Buffer.push values 1\n",
                    "    let frozen: Slice I32 = Buffer.freeze values\n",
                    "    Slice.length frozen\n",
                    "}\n",
                    "\n",
                    "def trapped: () -> USize = () => {\n",
                    "    let values: Buffer I32 = Buffer.with_capacity (1 satisfies USize)\n",
                    "    let out: Ref I32 = Buffer.get_ref values (4 satisfies USize)\n",
                    "    0\n",
                    "}\n",
                    "\n",
                    "let first = basics ()\n",
                    "let second = transfer ()\n",
                    "let third = clone_tags ()\n",
                    "let frozen = freeze ()\n",
                    "let trap = trapped ()\n",
                ),
                "ownership",
            ),
            &["basics", "transfer", "clone_tags", "freeze", "trapped"],
        ))),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "reactive_exits",
                concat!(
                    // `return`, `break`, and `continue` dispose the reactive
                    // scopes opened since their target, before the owned drops.
                    "def early: Bool -> I32 = flag => with Reactive = reactive_scope () {\n",
                    "    if (flag) { return 1 }\n",
                    "    0\n",
                    "}\n",
                    "\n",
                    "def broken: () -> I32 = () => loop {\n",
                    "    with Reactive = reactive_scope () { break 2 }\n",
                    "}\n",
                    "\n",
                    "def continued: () -> I32 = () => {\n",
                    "    let mut index = 0\n",
                    "    loop {\n",
                    "        index = index + 1\n",
                    "        if (index == 3) { break index }\n",
                    "        with Reactive = reactive_scope () { continue }\n",
                    "    }\n",
                    "}\n",
                    "\n",
                    "let first = early True\n",
                    "let second = early False\n",
                    "let third = broken ()\n",
                    "let fourth = continued ()\n",
                ),
                "ownership",
            ),
            &["early", "broken", "continued"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "ref_replace",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "type Payload = wrap CString\n",
                    "\n",
                    "def replace: () -> () = () => {\n",
                    "    let mut reference: Ref Payload = Ref (Payload (c_string \"original\"))\n",
                    "    let replaced: Payload = Ref.replace reference (Payload (c_string \"replacement\"))\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "replace ()\n",
                ),
                "ownership",
            ),
            &["replace"],
        )),
        "",
    ),
    // Every structural body, formatting delegates, and cleanup.
    must_run(expect_stdout(
        emits(
            inline(
                "structural_debug",
                r#"use std.cinterop.(CString, c_string)
extern "c" { puts: CString -> I32 }
type Held T = wrap (T)
impl<T where Debug T> Debug (Held T) { fmt = (Held value, mut formatter) => Debug.fmt (value, formatter) }
def nested: ((I32, I32), (I32, I32)) -> String = pair => "${pair:?}"
def named: (left: I32, right: I32) -> String = pair => "${pair:?}"
def sum: (I32 | U8) -> String = value => "${value:?}"
def generic: (Held I32, I32) -> String = pair => "${pair:?}"
def template: I32 -> String = value => "display=${value} debug=${value:?}"
puts (CString.from_string (nested ((1, 2), (3, 4))))
puts (CString.from_string (named (5, 6)))
puts (CString.from_string (sum (7 satisfies (I32 | U8))))
puts (CString.from_string (sum ((8 satisfies U8) satisfies (I32 | U8))))
puts (CString.from_string (generic (Held 9, 10)))
puts (CString.from_string (template 11))
def nested_sum: ((I32, I32) | U8) -> String = value => "${value:?}"
puts (CString.from_string (nested_sum ((12, 13) satisfies ((I32, I32) | U8))))
puts (CString.from_string (nested_sum ((14 satisfies U8) satisfies ((I32, I32) | U8))))
"#,
                "structural",
            ),
            &[
                "nested",
                "named",
                "sum",
                "generic",
                "template",
                "nested_sum",
            ],
        ),
        "((1, 2), (3, 4))\n(left: 5, right: 6)\n7\n8\n(9, 10)\ndisplay=11 debug=11\n(12, 13)\n14\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "structural_index_references",
                r#"use std.cinterop.CString
extern "c" { puts: CString -> I32 }
type Row = wrap (I32, I32)
impl Index Row USize I32 { index = (row, position) => 7 }
def mixed: (U8, I32) -> (I32 | U8) = pair => pair[1]
def uniform: (I32, I32) -> I32 = pair => pair[0]
def replace: (I32, I32) -> (I32, I32) = pair => { let mut own = pair; own[0] = 3; own }
def ref_uniform: Ref (I32, I32) -> I32 = reference => reference[1]
def ref_mixed: Ref (U8, I32) -> (I32 | U8) = reference => reference[0]
def ref_row: Ref Row -> I32 = reference => reference[0]
def ref_replace: move (Ref (I32, I32)) -> Ref (I32, I32) = move reference => { let mut own = reference; own[0] = 8; own }
puts (CString.from_string "mixed=${mixed ((1 satisfies U8), 2):?} uniform=${uniform (4, 5)}")
puts (CString.from_string "replace=${replace (1, 2):?}")
puts (CString.from_string "refs=${ref_uniform (Ref (5, 6))} ${ref_mixed (Ref ((9 satisfies U8), 10)):?} ${ref_row (Ref (Row (1, 2)))}")
let replaced = ref_replace (Ref (1, 2))
puts (CString.from_string "ref_replace=${replaced[0]} ${replaced[1]}")
"#,
                "structural",
            ),
            &[
                "mixed",
                "uniform",
                "replace",
                "ref_uniform",
                "ref_mixed",
                "ref_row",
                "ref_replace",
            ],
        ),
        "mixed=2 uniform=4\nreplace=(3, 2)\nrefs=6 9 7\nref_replace=8 2\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "structural_iterators",
                r#"use std.cinterop.CString
extern "c" { puts: CString -> I32 }
def walk_mixed: (U8, I32) -> () = pair => { for item in pair { puts (CString.from_string "${item:?}") }; () }
def walk_uniform: (I32, I32) -> () = pair => { for item in pair { puts (CString.from_string "${item}") }; () }
walk_mixed ((1 satisfies U8), 2)
walk_uniform (3, 4)
"#,
                "structural",
            ),
            &["walk_mixed", "walk_uniform"],
        ),
        "1\n2\n3\n4\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "structural_mutation_drop",
                r#"use std.cinterop.(CString, c_string)
extern "c" { puts: CString -> I32 }
type Tag = wrap CString
impl Drop Tag { drop = Tag text => { puts text; () } }
def replace_owned: move (Tag, Tag) -> () = move pair => {
    let mut own = pair
    own[0] = Tag (c_string "replacement")
    ()
}
replace_owned (Tag (c_string "old"), Tag (c_string "second"))
"#,
                "structural",
            ),
            &["replace_owned"],
        ),
        "old\nsecond\nreplacement\n",
    )),
    // Runtime coverage for a droppable element replaced through a reference and
    // for the structural bounds traps.
    must_run(expect_stdout(
        emits(
            inline(
                "structural_ref_mutation_drop",
                r#"use std.cinterop.(CString, c_string)
extern "c" { puts: CString -> I32 }
type Tag = wrap CString
impl Drop Tag { drop = Tag text => { puts text; () } }
def ref_mutate: move (Ref (Tag, Tag)) -> () = move reference => {
    let mut own = reference
    own[0] = Tag (c_string "ref replacement")
    ()
}
ref_mutate (Ref (Tag (c_string "ref old"), Tag (c_string "ref second")))
"#,
                "structural",
            ),
            &["ref_mutate"],
        ),
        "ref old\n",
    )),
    expect_stdout(
        must_run(expect_trap(emits(
            inline(
                "structural_switch_trap",
                r#"def at: ((U8, I32), USize) -> (I32 | U8) = (pair, position) => pair[position]
let value = at (((1 satisfies U8), 2), (5 satisfies USize))
"#,
                "structural",
            ),
            &["at"],
        ))),
        "",
    ),
    expect_stdout(
        must_run(expect_trap(emits(
            inline(
                "structural_deref_trap",
                r#"def at: (Ref (I32, I32), USize) -> I32 = (reference, position) => reference[position]
let value = at (Ref (1, 2), (5 satisfies USize))
"#,
                "structural",
            ),
            &["at"],
        ))),
        "",
    ),
    // Generic fixtures instantiate each artifact twice and print proof that
    // the second instantiation uses its own coroutine pair or runner.
    generic_artifact_fixture(
        expect_stdout(
            inline(
                "generic_coro_pair",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def generic: <T where Copy T> T -> Coroutine{} T = value => coro { value }\n",
                    "let a: Coroutine{} I32 = generic 7\n",
                    "let b: Coroutine{} U8 = generic (1 satisfies U8)\n",
                    "let x = block_on a\n",
                    "let y = block_on b\n",
                    "println \"a=${x:?} b=${y:?}\"\n",
                ),
                "coroutines",
            ),
            "a=7 b=1\n",
        ),
        CorpusGenericArtifacts::CoroutinePairs,
    ),
    // A concrete result type does not make generic capture layouts identical.
    // I32 and (U8, U8) captures need distinct coroutine pairs; otherwise the
    // second frame can be read through the first frame's incompatible layout.
    generic_artifact_fixture(
        expect_stdout(
            inline(
                "generic_coro_alias",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def generic: <T where Copy T, Debug T> T -> Coroutine{IO} I32 = value => coro {\n",
                    "    println \"${value:?}\"\n",
                    "    0\n",
                    "}\n",
                    "let a = generic (7 satisfies I32)\n",
                    "let b = generic ((1 satisfies U8), (2 satisfies U8))\n",
                    "let x = block_on a\n",
                    "let y = block_on b\n",
                ),
                "coroutines",
            ),
            "7\n(1, 2)\n",
        ),
        CorpusGenericArtifacts::CoroutinePairs,
    ),
    generic_artifact_fixture(
        expect_stdout(
            inline(
                "generic_reaction_runner",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def generic_reaction: <T where Copy T, Debug T> T ->{Reactive, IO} () =\n",
                    "  value => reaction { println \"reaction ${value:?}\"; () }\n",
                    "with Reactive = reactive_scope () {\n",
                    "  generic_reaction 7\n",
                    "  generic_reaction (1 satisfies U8)\n",
                    "}\n",
                ),
                "coroutines",
            ),
            "reaction 7\nreaction 1\n",
        ),
        CorpusGenericArtifacts::ReactionRunners,
    ),
    generic_artifact_fixture(
        expect_stdout(
            inline(
                "generic_until_runner",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "let signal count = 0\n",
                    "def peek: <T> [T] -> I32 = _ => 0\n",
                    "def generic_until: <T where Copy T, Debug T> T -> Coroutine{Reactive, IO} () =\n",
                    "  value => coro {\n",
                    "    let _ = await (until { count + peek value > 0 })\n",
                    "    println \"until ${value:?}\"\n",
                    "  }\n",
                    "count = 1\n",
                    "with Reactive = reactive_scope () {\n",
                    "  let a: Coroutine{Reactive, IO} () = generic_until 7\n",
                    "  let b: Coroutine{Reactive, IO} () = generic_until (1 satisfies U8)\n",
                    "  let _ = block_on a\n",
                    "  let _ = block_on b\n",
                    "}\n",
                ),
                "coroutines",
            ),
            "until 7\nuntil 1\n",
        ),
        CorpusGenericArtifacts::UntilRunners,
    ),
    generic_artifact_fixture(
        expect_stdout(
            inline(
                "generic_derived_runner",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "let signal count = 0\n",
                    "def peek: <T> [T] -> I32 = _ => 0\n",
                    "def generic_derived: <T where Copy T, Debug T> T ->{state.read, IO} I32 =\n",
                    "  value => {\n",
                    "    let local = count + peek value\n",
                    "    println \"derived ${value:?} ${local:?}\"\n",
                    "    local\n",
                    "  }\n",
                    "let a = generic_derived 7\n",
                    "let b = generic_derived (1 satisfies U8)\n",
                    "println \"results ${a:?} ${b:?}\"\n",
                ),
                "coroutines",
            ),
            "derived 7 0\nderived 1 0\nresults 0 0\n",
        ),
        CorpusGenericArtifacts::DerivedRunners,
    ),
    // Coroutine and reactive fixtures as runnable programs, plus the
    // cancellation drop-order fixture.
    must_run(expect_stdout(
        emits(
            inline(
                "coroutine_nested_child_awaits",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def leaf: () -> Coroutine{IO} I32 = () => coro { println \"leaf\"; 1 }\n",
                    "def middle: () -> Coroutine{IO} I32 = () => coro {\n",
                    "  let a = await (leaf ())\n",
                    "  let b = await (coro { println \"inner\"; 2 })\n",
                    "  a + b\n",
                    "}\n",
                    "let value = block_on (middle ())\n",
                    "println \"value ${value:?}\"\n",
                ),
                "coroutines",
            ),
            &["leaf", "middle"],
        ),
        "leaf\ninner\nvalue 3\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "coroutine_cancellation",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def worker: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                    "  println \"worker start\"\n",
                    "  let _ = await (yield_now ())\n",
                    "  println \"worker end\"\n",
                    "}\n",
                    "let sched = scheduler ()\n",
                    "with Tasks = task_scope (sched) {\n",
                    "  let doomed = spawn (worker ())\n",
                    "  let _ = pump (sched, 4)\n",
                    "  Task.cancel doomed\n",
                    "  let _ = pump (sched, 4)\n",
                    "  println \"finished ${Task.is_finished doomed:?}\"\n",
                    "}\n",
                ),
                "coroutines",
            ),
            &["worker"],
        ),
        "worker start\nfinished True\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "coroutine_parked_cancellation",
                concat!(
                    // Cancel coroutines parked on an unresolved `Wait` (the
                    // unwind abandons the record) and on an `until` child (the
                    // unwind runs the child's cleanup); neither body resumes.
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "let signal flag = 0\n",
                    "def waiter: move Wait I32 ->{IO} Coroutine{IO} () = move pending => coro {\n",
                    "  let outcome = await pending\n",
                    "  match outcome {\n",
                    "    Completed value => println \"wait ${value:?}\",\n",
                    "    Cancelled() => println \"wait cancelled\",\n",
                    "  }\n",
                    "}\n",
                    "def waiting: () -> Coroutine{Reactive, IO} () = () => coro {\n",
                    "  let _ = await (until { flag >= 1 })\n",
                    "  println \"until done\"\n",
                    "}\n",
                    "def make_completion: Scheduler -> (wait: Wait I32, resolver: Resolver I32) = s => completion s\n",
                    "let sched = scheduler ()\n",
                    "with Reactive = reactive_scope () {\n",
                    "  with Tasks = task_scope (sched) {\n",
                    "    let (wait, resolver) = make_completion sched\n",
                    "    let parked_wait = spawn (waiter wait)\n",
                    "    let parked_until = spawn (waiting ())\n",
                    "    let _ = pump (sched, 8)\n",
                    "    Task.cancel parked_wait\n",
                    "    Task.cancel parked_until\n",
                    "    let _ = pump (sched, 8)\n",
                    "    flag = 1\n",
                    "    let _ = pump (sched, 8)\n",
                    "    println \"wait finished ${Task.is_finished parked_wait:?}\"\n",
                    "    println \"until finished ${Task.is_finished parked_until:?}\"\n",
                    "  }\n",
                    "}\n",
                ),
                "coroutines",
            ),
            &["waiter", "waiting", "make_completion"],
        ),
        "wait finished True\nuntil finished True\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "coroutine_wait_and_until_states",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "let signal flag = 0\n",
                    "def waiter: move Wait I32 ->{IO} Coroutine{IO} () = move pending => coro {\n",
                    "  let outcome = await pending\n",
                    "  match outcome {\n",
                    "    Completed value => println \"wait ${value:?}\",\n",
                    "    Cancelled() => println \"wait cancelled\",\n",
                    "  }\n",
                    "}\n",
                    "def waiting: () -> Coroutine{Reactive, IO} () = () => coro {\n",
                    "  let _ = await (until { flag >= 1 })\n",
                    "  println \"until done\"\n",
                    "}\n",
                    "def make_completion: Scheduler -> (wait: Wait I32, resolver: Resolver I32) = s => completion s\n",
                    "let sched = scheduler ()\n",
                    "with Reactive = reactive_scope () {\n",
                    "  with Tasks = task_scope (sched) {\n",
                    "    let (wait, resolver) = make_completion sched\n",
                    "    let _ = spawn (waiter wait)\n",
                    "    let _ = spawn (waiting ())\n",
                    "    let _ = pump (sched, 8)\n",
                    "    Resolver.complete resolver 9\n",
                    "    flag = 1\n",
                    "    let _ = pump (sched, 8)\n",
                    "    let _ = pump (sched, 8)\n",
                    "  }\n",
                    "}\n",
                ),
                "coroutines",
            ),
            &["waiter", "waiting", "make_completion"],
        ),
        "wait 9\nuntil done\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "reaction_resource",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "type Counter = wrap (n: I32)\n",
                    "def read: () ->{Counter} I32 = () => (resource Counter).n\n",
                    "def subscribe: () ->{Reactive, Counter, IO} () = () => reaction { println \"count ${read ():?}\"; () }\n",
                    "with Counter = Counter (n: 5) {\n",
                    "  with Reactive = reactive_scope () {\n",
                    "    subscribe ()\n",
                    "  }\n",
                    "}\n",
                ),
                "coroutines",
            ),
            &["read", "subscribe"],
        ),
        "count 5\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "derived_initializer_and_instance",
                concat!(
                    "use std.io.(IO, println)\n",
                    "let signal count = 0\n",
                    "let doubled = count + count\n",
                    "def make: () ->{state.read, IO} I32 = () => {\n",
                    "  let local = count * 3\n",
                    "  println \"local ${local:?}\"\n",
                    "  local\n",
                    "}\n",
                    "count = 2\n",
                    "println \"doubled ${doubled:?}\"\n",
                    "let result = make ()\n",
                ),
                "coroutines",
            ),
            &["make"],
        ),
        "doubled 4\nlocal 6\n",
    )),
    must_run(expect_stdout(
        emits(
            inline(
                "derived_droppable_capture",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "use std.io.(IO, println)\n",
                    "extern \"c\" { strlen: CString -> USize }\n",
                    "let signal base = 0\n",
                    "def render: (CString, I32) -> String = (text, extra) => \"${strlen text}+${extra}\"\n",
                    "def make: move CString ->{state.read, IO} String = move text => {\n",
                    "  let derived = render (text, base)\n",
                    "  println \"derived ${derived}\"\n",
                    "  derived\n",
                    "}\n",
                    "base = 3\n",
                    "let value = make (c_string \"len\")\n",
                    "println \"value ${value}\"\n",
                ),
                "coroutines",
            ),
            &["render", "make"],
        ),
        "derived 3+3\nvalue 3+3\n",
    )),
    // generic: the cancel unwind drops the frame bindings in plan order
    // (`first` then `second`); emission fixed the completed-coroutine
    // leak, so the completed sibling's `leaked` frame binding is dropped when
    // that coroutine completes.
    must_run(expect_stdout(
        emits(
            inline(
                "coroutine_drop_order",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.cinterop.(CString, c_string)\n",
                    "use std.io.(IO, println)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Tag = wrap CString\n",
                    "impl Drop Tag { drop = Tag text => { puts text; () } }\n",
                    "def tag: move CString -> Tag = move text => Tag text\n",
                    "def with_tags: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                    "  let first = tag (c_string \"first\")\n",
                    "  let second = tag (c_string \"second\")\n",
                    "  println \"tags ready\"\n",
                    "  let _ = await (yield_now ())\n",
                    "  ()\n",
                    "}\n",
                    "def completing: () -> Coroutine{IO} () = () => coro {\n",
                    "  let kept = tag (c_string \"leaked\")\n",
                    "  println \"completing\"\n",
                    "}\n",
                    "let sched = scheduler ()\n",
                    "with Tasks = task_scope (sched) {\n",
                    "  let doomed = spawn (with_tags ())\n",
                    "  let _ = spawn (completing ())\n",
                    "  let _ = pump (sched, 8)\n",
                    "  Task.cancel doomed\n",
                    "  let _ = pump (sched, 8)\n",
                    "  println \"cancelled ${Task.is_finished doomed:?}\"\n",
                    "}\n",
                    "println \"scope closed\"\n",
                ),
                "coroutines",
            ),
            &["tag", "with_tags", "completing"],
        ),
        "tags ready\ncompleting\nleaked\nfirst\nsecond\ncancelled True\nscope closed\n",
    )),
    must_run(expect_stdout(
        inline(
            "slice_ref_coercion",
            "use std.slice.Slice\nlet fixed: Ref (I32; 3) = Ref (1, 2, 3)\nlet values: Slice I32 = fixed\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "local_recursive_cells",
            "def outer: () -> I32 = () => {\n def f: () -> I32 = () => g ()\n def g: () -> I32 = () => f ()\n f ()\n}\n",
            "emission",
        ),
        "",
    )),
    // Regressions for stored-closure state slots, captured mutable field
    // initialization checks, and effect-specialized await/until bodies.
    must_run(expect_stdout(
        inline(
            "local_recursive_call_state_slot",
            "def outer = value: I32 => {\n  def recurse: I32 -> I32 = n => {\n    let captured = value\n    recurse n\n  }\n  recurse 1\n}\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "captured_mut_field_write",
            "def make = () => {\n  let mut point = (x: 1, y: 2)\n  let update = () => { point.x = point.x + 1; point.x }\n  update ()\n}\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "await_task_effect_pair",
            "use std.coroutine.*\nuse std.io.(IO, println)\ndef leaf: () -> Coroutine{} I32 = () => coro { 9 }\ndef waiter: () -> Coroutine{Tasks, IO} I32 = () => coro {\n    let t = spawn (leaf ())\n    let r = await t\n    match r {\n        Completed v => v,\n        Cancelled() => 0,\n    }\n}\nlet sched = scheduler ()\nwith Tasks = task_scope (sched) {\n    let _ = spawn (waiter ())\n    let _ = pump (sched, 8)\n}\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "local_generic_cells",
            "def outer: () -> I32 = () => {\n def recur: <T> T -> T = value => recur value\n recur 1\n}\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "natural_repeated_return",
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\nlet repeated: (I32; 3) = repeat 7 3\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "effect_closure_resources",
            "use std.io.(IO, println)\ndef twice: <effect E> (() ->{E} ()) ->{E} () = f => { f (); f () }\ndef output: () ->{IO} () = () => println \"hello\"\ntwice output\n",
            "emission",
        ),
        "hello\nhello\n",
    )),
    must_run(expect_stdout(
        inline(
            "product_trait_argument",
            "trait Merge Left Right Output { merge: (Left, Right) -> Output }\nimpl Merge I32 I32 I32 { merge = (left, right) => left + right }\ndef combine: <L, R, O where Merge L R O> (L, R) -> O = pair => Merge.merge pair\nlet total: I32 = combine (20, 22)\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "nested_expression_return",
            "def identity = (value: I32) => value\ndef answer = () => { identity { return 42; }; 0; }\nanswer ()\n",
            "emission",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "sibling_initializer_names",
            "let a: I32 = { mod foo { pub let value: I32 = 1 }; foo.value }\nlet b: I32 = { mod foo { pub let value: I32 = 2 }; foo.value }\n",
            "emission",
        ),
        "",
    )),
    expect_stdout(
        inline(
            "block_tail_coroutine",
            "use std.coroutine.*\ndef f: () -> Coroutine{} I32 = () => { coro { 42 } }\nlet held = f ()\n",
            "emission",
        ),
        "",
    ),
    // Parenthesized product rule: a plain one-element product `(e)` is its element even
    // when `e`'s own type is a product (`()` or a pair); lowering used to read
    // that type as the parenthesized product's layout and reject the program.
    must_run(expect_stdout(
        emits(
            inline(
                "parenthesized_singletons",
                concat!(
                    "use std.io.(IO, println)\n",
                    "def nothing: () -> () = () => ()\n",
                    "def pair: () -> (I32, I32) = () => (1, 2)\n",
                    "def show: <T where Copy T, Debug T> T ->{IO} () = value => {\n",
                    "    let same = (value)\n",
                    "    println \"${same:?}\"\n",
                    "}\n",
                    "(println \"effectful\")\n",
                    "let unit = (nothing ())\n",
                    "let empty = (())\n",
                    "let (a, b) = (pair ())\n",
                    "let p = (3, 4)\n",
                    "let (c, d) = (p)\n",
                    "println \"${a:?} ${b:?} ${c:?} ${d:?}\"\n",
                    "show (5, 6)\n",
                    "show 7\n",
                    "show ()\n",
                ),
                "emission",
            ),
            &["show"],
        ),
        "effectful\n1 2 3 4\n(5, 6)\n7\n()\n",
    )),
    // Every route that reaches an extern callable value receives closure-shaped
    // parameters, so the adapter loads a borrowed `CString` before the native
    // call; the caller releases the temporary.
    must_run(expect_stdout(
        emits(
            inline(
                "extern_adapter_abi",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "def evaluate: (() -> I32) -> I32 = callback => callback ()\n",
                    "def call_it: (CString -> I32) -> I32 = callback => callback (c_string \"callback\")\n",
                    "def direct: CString -> I32 = value => puts value\n",
                    "def value: () -> (CString -> I32) = () => puts\n",
                    "def captured: () -> (() -> I32) = () => {\n",
                    "    let adapter = puts\n",
                    "    () => adapter (c_string \"closure\")\n",
                    "}\n",
                    "def thunk_env: move CString -> I32 = move value => evaluate { puts value }\n",
                    "def thunk_temporary: () -> I32 = () => evaluate { puts (c_string \"temporary\"); 1 }\n",
                    "let first = call_it direct\n",
                    "let adapter = value ()\n",
                    "let second = adapter (c_string \"value\")\n",
                    "let third = captured ()\n",
                    "let fourth = third ()\n",
                    "let fifth = thunk_env (c_string \"thunk\")\n",
                    "let sixth = thunk_temporary ()\n",
                ),
                "regressions",
            ),
            &[
                "evaluate",
                "call_it",
                "direct",
                "value",
                "captured",
                "thunk_env",
                "thunk_temporary",
            ],
        ),
        "callback\nvalue\nclosure\nthunk\ntemporary\n",
    )),
    // A field write resolves to its base's signal for notification, so a
    // reaction over a signal product field re-runs. A field projection never
    // writes initialization state; the base is already initialized when the
    // projection executes.
    must_run(expect_stdout(
        emits(
            inline(
                "signal_field_writes",
                concat!(
                    "use std.io.(IO, println)\n",
                    "let signal point = (x: 0, y: 0)\n",
                    "reaction {\n",
                    "    println \"seen ${point.x}\"\n",
                    "}\n",
                    "point.x = 5\n",
                    "point.x = 7\n",
                    "let signal outer = (left: (x: 0, y: 0), right: 0)\n",
                    "reaction {\n",
                    "    println \"nested ${outer.left.x}\"\n",
                    "}\n",
                    "outer.left.x = 4\n",
                    "def captured = () => {\n",
                    "    let mut local = (x: 1, y: 2)\n",
                    "    let update = () => { local.x = 9 }\n",
                    "    update ()\n",
                    "    local.x\n",
                    "}\n",
                    "println \"captured ${captured ()}\"\n",
                ),
                "regressions",
            ),
            &["captured"],
        ),
        "seen 0\nseen 5\nseen 7\nnested 0\nnested 4\ncaptured 9\n",
    )),
    // An early exit closes every task scope opened since its target, right
    // after reactive disposal and before owned drops. A `break` out of the loop
    // abandons the inner scope, cancelling its child before its next resume.
    must_run(expect_stdout(
        emits(
            inline(
                "task_scope_break_exit",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def worker: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                    "    println \"task start\"\n",
                    "    let _ = await (yield_now ())\n",
                    "    println \"task end\"\n",
                    "}\n",
                    "def abandoning: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                    "    let sched = scheduler ()\n",
                    "    let handle = loop {\n",
                    "        with Tasks = task_scope (sched) {\n",
                    "            let child = spawn (worker ())\n",
                    "            let _ = pump (sched, 1)\n",
                    "            break child\n",
                    "        }\n",
                    "    }\n",
                    "    println \"break finished ${Task.is_finished handle:?}\"\n",
                    "    ()\n",
                    "}\n",
                    "let sched = scheduler ()\n",
                    "with Tasks = task_scope (sched) {\n",
                    "    let _ = spawn (abandoning ())\n",
                    "    let _ = pump (sched, 16)\n",
                    "}\n",
                    "println \"done\"\n",
                ),
                "regressions",
            ),
            &["worker", "abandoning"],
        ),
        "task start\nbreak finished True\ndone\n",
    )),
    // The same cancellation through `continue`: the abandoned child never
    // reaches its trailing output.
    must_run(expect_stdout(
        emits(
            inline(
                "task_scope_continue_exit",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def worker: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                    "    println \"task start\"\n",
                    "    let _ = await (yield_now ())\n",
                    "    println \"task end\"\n",
                    "}\n",
                    "def continuing: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                    "    let sched = scheduler ()\n",
                    "    let mut index = 0\n",
                    "    loop {\n",
                    "        index = index + 1\n",
                    "        with Tasks = task_scope (sched) {\n",
                    "            let child = spawn (worker ())\n",
                    "            let _ = pump (sched, 1)\n",
                    "            if (index == 1) { continue }\n",
                    "            println \"second finished ${Task.is_finished child:?}\"\n",
                    "            break\n",
                    "        }\n",
                    "    }\n",
                    "    ()\n",
                    "}\n",
                    "let sched = scheduler ()\n",
                    "with Tasks = task_scope (sched) {\n",
                    "    let _ = spawn (continuing ())\n",
                    "    let _ = pump (sched, 16)\n",
                    "}\n",
                    "println \"done\"\n",
                ),
                "regressions",
            ),
            &["worker", "continuing"],
        ),
        "task start\nsecond finished False\ndone\n",
    )),
    // `return` closes the open scope before the owned drops, cancelling the
    // child it spawned: after the return, pumping the scheduler never reaches
    // the child's trailing output. (An ordinary function can spawn since the
    // `spawn` effect-variable fix.)
    must_run(expect_stdout(
        inline(
            "task_scope_return_exit",
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def worker: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                "    println \"task start\"\n",
                "    let _ = await (yield_now ())\n",
                "    println \"task end\"\n",
                "}\n",
                "def early: Scheduler ->{IO} I32 = sched => {\n",
                "    with Tasks = task_scope (sched) {\n",
                "        let child = spawn (worker ())\n",
                "        let _ = pump (sched, 1)\n",
                "        return 1\n",
                "    }\n",
                "    0\n",
                "}\n",
                "let sched = scheduler ()\n",
                "println \"returned ${early sched}\"\n",
                "let _ = pump (sched, 16)\n",
                "println \"done\"\n",
            ),
            "regressions",
        ),
        "task start\nreturned 1\ndone\n",
    )),
    // A coroutine that completes normally drops its live frame bindings in plan
    // order, before the result is published. The cell state skips a moved-out
    // or never-initialized binding, a cancelled coroutine still drops exactly
    // once through the unwind, and a child-awaited coroutine drops its own
    // frame bindings on completion.
    must_run(expect_stdout(
        emits(
            inline(
                "coroutine_completion_drops",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.cinterop.(CString, c_string)\n",
                    "use std.io.(IO, println)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Tag = wrap CString\n",
                    "impl Drop Tag { drop = Tag text => { puts text; () } }\n",
                    "def tag: move CString -> Tag = move text => Tag text\n",
                    "def completes: () -> Coroutine{} () = () => coro {\n",
                    "    let kept = tag (c_string \"complete\")\n",
                    "    ()\n",
                    "}\n",
                    "def moved_out: () -> Coroutine{} () = () => coro {\n",
                    "    let first = tag (c_string \"moved\")\n",
                    "    let second = first\n",
                    "    ()\n",
                    "}\n",
                    "def never_ran: Bool -> Coroutine{} () = condition => coro {\n",
                    "    if (condition) {\n",
                    "        let never = tag (c_string \"never\")\n",
                    "        ()\n",
                    "    }\n",
                    "}\n",
                    "def cancelled: () -> Coroutine{Tasks} () = () => coro {\n",
                    "    let doomed = tag (c_string \"cancel\")\n",
                    "    let _ = await (yield_now ())\n",
                    "    ()\n",
                    "}\n",
                    "def child: () -> Coroutine{} Tag = () => coro {\n",
                    "    let held = tag (c_string \"child\")\n",
                    "    held\n",
                    "}\n",
                    "def parent: () -> Coroutine{} () = () => coro {\n",
                    "    let outer = tag (c_string \"parent\")\n",
                    "    let result = await (child ())\n",
                    "    ()\n",
                    "}\n",
                    "let first = block_on (completes ())\n",
                    "let second = block_on (moved_out ())\n",
                    "let third = block_on (never_ran False)\n",
                    "let sched = scheduler ()\n",
                    "with Tasks = task_scope (sched) {\n",
                    "    let doomed = spawn (cancelled ())\n",
                    "    let _ = pump (sched, 1)\n",
                    "    Task.cancel doomed\n",
                    "    let _ = pump (sched, 1)\n",
                    "    let _ = spawn (parent ())\n",
                    "    let _ = pump (sched, 8)\n",
                    "}\n",
                    "println \"done\"\n",
                ),
                "regressions",
            ),
            &[
                "tag",
                "completes",
                "moved_out",
                "never_ran",
                "cancelled",
                "child",
                "parent",
            ],
        ),
        "complete\nmoved\ncancel\nparent\nchild\ndone\n",
    )),
    // A generic `Drop` implementation applies by header unification plus bound
    // discharge. The conditional `Copy T` bound holds for `Box I32`/`Box (I32,
    // I32)` (user drop runs) and fails for `Box CString` (no user drop; the
    // `CString` is still freed), including nested products and sums.
    must_run(expect_stdout(
        emits(
            inline(
                "generic_drop_selection",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "use std.io.(IO, println)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Box T = wrap (T)\n",
                    "impl<T where Copy T> Drop (Box T) { drop = Box value => { puts (c_string \"copy box\"); () } }\n",
                    "def run_i32: () -> () = () => { let b = Box 2; () }\n",
                    "def run_product: () -> () = () => { let b = Box (1, 2); () }\n",
                    "def run_cstring: () -> () = () => { let b = Box (c_string \"free me\"); () }\n",
                    "def run_nested: () -> () = () => {\n",
                    "    let b: (Box I32, Box CString) = (Box 3, Box (c_string \"x\"))\n",
                    "    ()\n",
                    "}\n",
                    "def run_sum: Bool -> () = condition => {\n",
                    "    let value: (Box I32 | Box CString) = when { condition => Box 4, else => Box (c_string \"y\") }\n",
                    "    ()\n",
                    "}\n",
                    "run_i32 ()\n",
                    "run_product ()\n",
                    "run_cstring ()\n",
                    "run_nested ()\n",
                    "run_sum True\n",
                    "run_sum False\n",
                    "println \"done\"\n",
                ),
                "regressions",
            ),
            &[
                "run_i32",
                "run_product",
                "run_cstring",
                "run_nested",
                "run_sum",
            ],
        ),
        "copy box\ncopy box\ncopy box\ncopy box\ndone\n",
    )),
    // A generic `Drop` at two instantiations drops through its own instance:
    // an owned local, a moved-out local (dropped once), a returned value, a
    // closure capture, a coroutine frame binding, and a drop body whose
    // parameter is not owned by the method (no double drop of `Inner`).
    must_run(expect_stdout(
        emits(
            inline(
                "generic_drop_ownership",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.cinterop.(CString, c_string)\n",
                    "use std.io.(IO, println)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Wrapper T = wrap (T)\n",
                    "impl<T> Drop (Wrapper T) { drop = Wrapper value => { puts (c_string \"wrapper\"); () } }\n",
                    "def wrap: <T> move T -> Wrapper T = move value => Wrapper value\n",
                    "def run_i32: () -> () = () => { let w = wrap 1; () }\n",
                    "def run_cstring: () -> () = () => { let w = wrap (c_string \"x\"); () }\n",
                    "def moved_out: () -> () = () => {\n",
                    "    let first = wrap (c_string \"moved\")\n",
                    "    let second = first\n",
                    "    ()\n",
                    "}\n",
                    "def returns: () -> Wrapper (CString) = () => {\n",
                    "    let held = wrap (c_string \"returned\")\n",
                    "    held\n",
                    "}\n",
                    "def consumes: () -> () = () => {\n",
                    "    let value = returns ()\n",
                    "    ()\n",
                    "}\n",
                    "def keeper: move (Wrapper I32) -> (() -> I32) = move value => () => 1\n",
                    "def closure: () -> () = () => {\n",
                    "    let held = wrap 5\n",
                    "    let peek = keeper held\n",
                    "    peek ()\n",
                    "    ()\n",
                    "}\n",
                    "def coroutine: () -> Coroutine{} () = () => coro {\n",
                    "    let held = wrap 6\n",
                    "    ()\n",
                    "}\n",
                    "type Inner = wrap CString\n",
                    "impl Drop Inner { drop = Inner text => { puts text; () } }\n",
                    "type Outer T = wrap (T)\n",
                    "impl<T> Drop (Outer T) { drop = Outer value => { puts (c_string \"outer\"); () } }\n",
                    "def outer: <T> move T -> Outer T = move value => Outer value\n",
                    "def owned_parameter: () -> () = () => {\n",
                    "    let value = outer (Inner (c_string \"inner\"))\n",
                    "    ()\n",
                    "}\n",
                    "run_i32 ()\n",
                    "run_cstring ()\n",
                    "moved_out ()\n",
                    "consumes ()\n",
                    "closure ()\n",
                    "let first = block_on (coroutine ())\n",
                    "owned_parameter ()\n",
                    "println \"done\"\n",
                ),
                "regressions",
            ),
            &[
                "wrap",
                "run_i32",
                "run_cstring",
                "moved_out",
                "returns",
                "consumes",
                "keeper",
                "closure",
                "coroutine",
                "outer",
                "owned_parameter",
            ],
        ),
        "wrapper\nwrapper\nwrapper\nwrapper\nwrapper\nwrapper\nouter\ninner\ndone\n",
    )),
    // A coroutine's declared row bounds its body, as a function declaration's
    // does: over-declared coroutines take the declared row and run when
    // awaited, spawned, and driven by `block_on`.
    must_run(expect_stdout(
        emits(
            inline(
                "over_declared_coroutines",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "def quiet: () -> Coroutine{Tasks, IO} I32 = () => coro { 5 }\n",
                    "def outer: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                    "    let v = await (quiet ())\n",
                    "    println \"awaited ${v:?}\"\n",
                    "}\n",
                    "def wide: () -> Coroutine{IO} I32 = () => coro { 3 }\n",
                    "let sched = scheduler ()\n",
                    "with Tasks = task_scope (sched) {\n",
                    "    let _ = spawn (outer ())\n",
                    "    let _ = spawn (quiet ())\n",
                    "    let _ = pump (sched, 8)\n",
                    "}\n",
                    "println \"block_on ${block_on (wide ()):?}\"\n",
                ),
                "regressions",
            ),
            &["quiet", "outer", "wide"],
        ),
        "awaited 5\nblock_on 3\n",
    )),
    // A temporary passed to a borrowed parameter is owned by the call site and
    // dropped right after the call, on every route; a `move` parameter's
    // callee drops it instead. A borrowed named binding and the elements of a
    // spread named product stay with their owner until scope exit, while the
    // borrowed elements of a spread temporary are dropped after the call.
    must_run(expect_stdout(
        emits(
            inline(
                "borrowed_temporaries_drop_after_call",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Tag = wrap CString\n",
                    "impl Drop Tag { drop = Tag text => { puts text; () } }\n",
                    "trait Measure T { measure: T -> I32 }\n",
                    "impl Measure Tag { measure = tag => 1 }\n",
                    "def borrow: Tag -> I32 = tag => 1\n",
                    "def take: move Tag -> I32 = move tag => 2\n",
                    "def pair: (Tag, I32) -> I32 = (tag, count) => count\n",
                    "def both: (Tag, Tag) -> I32 = (first, second) => 2\n",
                    "def apply: (Tag -> I32) -> I32 = f => f (Tag (c_string \"drop indirect borrowed\"))\n",
                    "def apply_move: ((move Tag) -> I32) -> I32 = f => f (Tag (c_string \"drop indirect moved\"))\n",
                    "def mixed: (move Tag, Tag) -> I32 = (move first, second) => 2\n",
                    "def generic: <T> (T, T) -> I32 = (left, right) => 2\n",
                    "def make: () -> (Tag, Tag) = () => (Tag (c_string \"drop spread left\"), Tag (c_string \"drop spread right\"))\n",
                    "def run: () -> () = () => {\n",
                    "    puts (c_string \"direct borrowed\");\n",
                    "    borrow (Tag (c_string \"drop direct borrowed\"));\n",
                    "    puts (c_string \"direct moved\");\n",
                    "    take (Tag (c_string \"drop direct moved\"));\n",
                    "    puts (c_string \"indirect borrowed\");\n",
                    "    apply borrow;\n",
                    "    puts (c_string \"indirect moved\");\n",
                    "    apply_move take;\n",
                    "    puts (c_string \"trait borrowed\");\n",
                    "    Measure.measure (Tag (c_string \"drop trait borrowed\"));\n",
                    "    puts (c_string \"product element\");\n",
                    "    pair (Tag (c_string \"drop product element\"), 3);\n",
                    "    puts (c_string \"named binding\");\n",
                    "    let named = Tag (c_string \"drop named at scope exit\")\n",
                    "    borrow named;\n",
                    "    let spread = (Tag (c_string \"drop spread second\"), Tag (c_string \"drop spread first\"))\n",
                    "    both (...spread);\n",
                    "    puts (c_string \"spread temporary\");\n",
                    "    both (...make ());\n",
                    "    puts (c_string \"spread temporary mixed\");\n",
                    "    mixed (...make ());\n",
                    "    puts (c_string \"spread temporary generic\");\n",
                    "    generic (...make ());\n",
                    "    puts (c_string \"extern C string\");\n",
                    "    puts (c_string \"printed by puts\");\n",
                    "    puts (c_string \"end of scope\");\n",
                    "    ()\n",
                    "}\n",
                    "let _ = run ()\n",
                ),
                "ownership",
            ),
            &[
                "run",
                "apply",
                "apply_move",
                "borrow",
                "take",
                "pair",
                "both",
                "mixed",
                "make",
            ],
        ),
        "direct borrowed\ndrop direct borrowed\ndirect moved\ndrop direct moved\nindirect borrowed\ndrop indirect borrowed\nindirect moved\ndrop indirect moved\ntrait borrowed\ndrop trait borrowed\nproduct element\ndrop product element\nnamed binding\nspread temporary\ndrop spread right\ndrop spread left\nspread temporary mixed\ndrop spread left\ndrop spread right\nspread temporary generic\ndrop spread right\ndrop spread left\nextern C string\nprinted by puts\nend of scope\ndrop spread first\ndrop spread second\ndrop named at scope exit\n",
    )),
    // A `Ref` into the middle of a managed allocation keeps the whole
    // allocation alive across collections: an array viewed as a slice, a
    // buffer, and a frozen buffer. No element is finalized during the churn.
    must_run(expect_stdout(
        emits(
            inline(
                "interior_refs_keep_their_allocation_alive",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "use std.slice.Slice\n",
                    "use std.buffer.Buffer\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Tag = wrap CString\n",
                    "impl Drop Tag { drop = Tag text => { puts text; () } }\n",
                    "def from_array: () -> Ref Tag = () => {\n",
                    "    let values: Slice Tag = Ref (Tag (c_string \"drop array first\"), Tag (c_string \"drop array second\"), Tag (c_string \"drop array third\"))\n",
                    "    Slice.get_ref values 2\n",
                    "}\n",
                    "def from_buffer: () -> Ref Tag = () => {\n",
                    "    let mut buffer: Buffer Tag = Buffer.with_capacity 4\n",
                    "    Buffer.push buffer (Tag (c_string \"drop buffer first\"))\n",
                    "    Buffer.push buffer (Tag (c_string \"drop buffer second\"))\n",
                    "    Buffer.get_ref buffer 1\n",
                    "}\n",
                    "def from_frozen: () -> Ref Tag = () => {\n",
                    "    let mut buffer: Buffer Tag = Buffer.with_capacity 4\n",
                    "    Buffer.push buffer (Tag (c_string \"drop frozen first\"))\n",
                    "    Buffer.push buffer (Tag (c_string \"drop frozen second\"))\n",
                    "    let frozen = Buffer.freeze buffer\n",
                    "    Slice.get_ref frozen 1\n",
                    "}\n",
                    "def churn: I32 -> I32 = n => {\n",
                    "    let mut index = 0\n",
                    "    loop {\n",
                    "        if (index == n) { break index }\n",
                    "        let garbage = \"x\" + \"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\"\n",
                    "        index = index + 1\n",
                    "        continue\n",
                    "    }\n",
                    "}\n",
                    "let array = from_array ()\n",
                    "let buffered = from_buffer ()\n",
                    "let frozen = from_frozen ()\n",
                    "let spins = churn 100000\n",
                    "puts (c_string \"after churn\")\n",
                    "let first = Ref.replace array (Tag (c_string \"drop replacement\"))\n",
                    "let second = Ref.replace buffered (Tag (c_string \"drop replacement\"))\n",
                    "let third = Ref.replace frozen (Tag (c_string \"drop replacement\"))\n",
                    "puts (c_string \"replaced\")\n",
                ),
                "ownership",
            ),
            &["from_array", "from_buffer", "from_frozen", "churn"],
        ),
        "after churn\nreplaced\n",
    )),
    // Taking a buffer element `Ref` costs nothing at collection time, so a
    // loop that takes many of them while allocating stays linear.
    must_run(expect_stdout(
        emits(
            inline(
                "buffer_refs_in_a_loop_stay_linear",
                concat!(
                    "use std.io.println\n",
                    "use std.buffer.Buffer\n",
                    "def run: USize -> USize = n => {\n",
                    "    let mut buffer: Buffer U8 = Buffer.with_capacity 16\n",
                    "    Buffer.push buffer 7;\n",
                    "    let mut index: USize = 0\n",
                    "    let mut total: USize = 0\n",
                    "    loop {\n",
                    "        if (index == n) { break total }\n",
                    "        let Ref value = Buffer.get_ref buffer 0\n",
                    "        let garbage = \"x\" + \"y\"\n",
                    "        total = total + 1\n",
                    "        index = index + 1\n",
                    "        continue\n",
                    "    }\n",
                    "}\n",
                    "println \"${run 100000:?}\"\n",
                ),
                "ownership",
            ),
            &["run"],
        ),
        "100000\n",
    )),
];

/// Extract every `define`d function body from one module's IR text, keyed by
/// the function's final symbol name.
#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use inkwell::context::Context;

    use crate::{LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker};

    use super::{CorpusProgram, CorpusSource, codegen_corpus};

    fn workspace_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent")
    }

    /// Freeze CLI inputs for the emission IR identity gate.
    #[doc(hidden)]
    fn dump_corpus(destination: &Path) {
        fn copy_sources(source: &Path, destination: &Path) {
            std::fs::create_dir_all(destination).unwrap();
            for entry in std::fs::read_dir(source).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let target = destination.join(entry.file_name());
                if path.is_dir() {
                    copy_sources(&path, &target);
                } else if path.extension().is_some_and(|extension| extension == "sta") {
                    std::fs::copy(path, target).unwrap();
                }
            }
        }

        for program in codegen_corpus() {
            let directory = destination.join(program.name);
            std::fs::create_dir_all(&directory).unwrap();
            match program.source {
                CorpusSource::Inline(source) => {
                    std::fs::write(directory.join("main.sta"), source).unwrap();
                }
                CorpusSource::File(path) => {
                    let source = workspace_root().join(path);
                    copy_sources(source.parent().unwrap(), &directory);
                    std::fs::copy(source, directory.join("main.sta")).unwrap();
                }
            }
        }
        eprintln!("dumped {} corpus programs", codegen_corpus().len());
    }

    #[test]
    #[ignore = "set STAPLE_CORPUS_DUMP to the destination directory"]
    fn dump_corpus_sources() {
        let destination = std::path::PathBuf::from(
            std::env::var_os("STAPLE_CORPUS_DUMP").expect("set STAPLE_CORPUS_DUMP"),
        );
        dump_corpus(&destination);
    }

    /// The program source and the directory its `use` paths resolve against:
    /// a file entry resolves relative to its own directory (so a directory of
    /// side modules works), an inline entry against the workspace root.
    fn program_source(program: &CorpusProgram) -> (String, std::path::PathBuf) {
        match program.source {
            CorpusSource::Inline(source) => (source.to_owned(), workspace_root().to_path_buf()),
            CorpusSource::File(path) => {
                let full = workspace_root().join(path);
                let source = std::fs::read_to_string(&full)
                    .unwrap_or_else(|error| panic!("`{path}` should read: {error}"));
                let root = full
                    .parent()
                    .expect("a corpus file has a parent directory")
                    .to_path_buf();
                (source, root)
            }
        }
    }

    fn lower(source: &str, root: &Path) -> LoweredModule {
        let program = ProgramLoader::new()
            .with_standard_library_root(workspace_root().join("stdlib"))
            .load_source(source, root)
            .unwrap_or_else(|diagnostics| panic!("source should load: {diagnostics:?}\n{source}"));
        let resolved = NameResolver::new()
            .resolve_program(program)
            .unwrap_or_else(|diagnostics| {
                panic!("source should resolve: {diagnostics:?}\n{source}")
            });
        let module = TypeChecker::new()
            .check(resolved)
            .unwrap_or_else(|diagnostics| {
                panic!("source should type check: {diagnostics:?}\n{source}")
            });
        Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("source should lower: {diagnostics:?}\n{source}"))
    }

    /// Strictly emit the full corpus and check catalog definitions, focus instances,
    /// structural coverage, and distinct generic artifacts.
    #[test]
    fn corpus_emits_catalog_definitions() {
        let mut structural_kinds = std::collections::HashSet::new();
        let mut structural_bodies = std::collections::HashSet::new();
        let mut defined = 0;
        // Every runnable entry pins stdout and, for a trap, the missing exit
        // status. These expectations detect changes in observable behavior.
        for program in codegen_corpus() {
            if matches!(program.expectation, super::CorpusExpectation::MustRun) {
                assert!(
                    program.expected_stdout.is_some(),
                    "runnable corpus entry `{}` does not pin its output",
                    program.name
                );
            }
        }
        for program in codegen_corpus() {
            let (source, root) = program_source(program);
            let lowered = lower(&source, &root);
            if program.topic == "structural" {
                for (_, artifact) in lowered.program().artifacts() {
                    if let Some(crate::LoweredArtifactPlan::StructuralMethod(plan)) = &artifact.plan
                    {
                        structural_kinds.insert(plan.structural);
                        use crate::StructuralBody;
                        structural_bodies.insert(match &plan.body {
                            StructuralBody::ProductDebug { .. } => "ProductDebug",
                            StructuralBody::SumDebug { .. } => "SumDebug",
                            StructuralBody::IndexSwitch { .. } => "IndexSwitch",
                            StructuralBody::IndexLoad { .. } => "IndexLoad",
                            StructuralBody::MutateReplace { .. } => "MutateReplace",
                            StructuralBody::DerefIndexLoad { .. } => "DerefIndexLoad",
                            StructuralBody::DerefDelegate { .. } => "DerefDelegate",
                            StructuralBody::IntoIterator { .. } => "IntoIterator",
                            StructuralBody::Next { .. } => "Next",
                            StructuralBody::Unexpanded => {
                                panic!("closed structural plan is unexpanded")
                            }
                        });
                    }
                }
            }
            let context = Context::create();
            let emitted = crate::codegen::lowered_emissions(&context, &lowered).unwrap_or_else(
                |diagnostics| {
                    panic!(
                        "strict lowered emission should succeed for `{}`: {diagnostics:?}\n{source}",
                        program.name
                    )
                },
            );
            if program.generic_artifacts.is_some() {
                assert_distinct_d5_artifacts(program, &lowered);
            }
            crate::lower::census::assert_catalog_census(program.name, &lowered, &emitted);
            assert_focus_emissions(program, &lowered, &emitted.defined_functions);
            crate::lower::census::assert_entry_block_allocas(program.name, &emitted);
            defined += emitted.defined_functions.len();
        }

        use crate::StructuralTraitMethod;
        for kind in [
            StructuralTraitMethod::Debug,
            StructuralTraitMethod::Index,
            StructuralTraitMethod::MutateIndex,
            StructuralTraitMethod::DerefIndex,
            StructuralTraitMethod::DerefMutateIndex,
            StructuralTraitMethod::IntoIterator,
            StructuralTraitMethod::Iterator,
        ] {
            assert!(
                structural_kinds.contains(&kind),
                "structural corpus misses {kind:?}"
            );
        }
        for body in [
            "ProductDebug",
            "SumDebug",
            "IndexSwitch",
            "IndexLoad",
            "MutateReplace",
            "DerefIndexLoad",
            "DerefDelegate",
            "IntoIterator",
            "Next",
        ] {
            assert!(
                structural_bodies.contains(body),
                "structural corpus misses {body}"
            );
        }

        eprintln!("codegen corpus: {defined} catalog definitions checked");
        assert!(defined > 0, "the corpus must define functions");
    }

    /// The catalog census rejects an unplanned function, a
    /// missing catalog function, and a mistyped declaration.
    #[test]
    fn catalog_census_rejects_corrupted_emissions() {
        use crate::lower::census::assert_catalog_census;
        let lowered = lower(
            "def twice: I32 -> I32 = value => value + value\nlet answer = twice 21\n",
            &workspace_root(),
        );
        let context = Context::create();
        let emitted = crate::codegen::lowered_emissions(&context, &lowered).expect("strict");
        assert_catalog_census("clean", &lowered, &emitted);
        let name = lowered
            .program()
            .instances()
            .find_map(|(id, instance)| {
                (instance.body.is_some())
                    .then(|| lowered.program().planned_name(id).map(str::to_owned))
                    .flatten()
                    .filter(|name| emitted.defined_functions.contains(name))
            })
            .expect("a defined instance");
        let corrupt = |mutate: &dyn Fn(&mut crate::codegen::LoweredEmissions)| {
            let mut copy = emitted.clone();
            mutate(&mut copy);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                assert_catalog_census("corrupt", &lowered, &copy)
            }))
            .expect_err("a corrupted module must fail the census")
        };
        let message = |payload: Box<dyn std::any::Any + Send>| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_default()
        };
        assert!(
            message(corrupt(&|copy| {
                copy.defined_functions.insert("unplanned".to_owned());
            }))
            .contains("no catalog entry plans")
        );
        assert!(
            message(corrupt(&|copy| {
                copy.defined_functions.remove(&name);
            }))
            .contains("neither defined nor explained")
        );
        assert!(
            message(corrupt(&|copy| {
                copy.function_types
                    .insert(name.clone(), "void ()".to_owned());
            }))
            .contains("catalog signature")
        );
    }

    /// Every instance of an entry's `emits` templates is
    /// defined with its planned catalog name. A template must exist in the module
    /// catalog and have at least one materialized instance.
    fn assert_focus_emissions(
        program: &CorpusProgram,
        lowered: &LoweredModule,
        defined: &HashSet<String>,
    ) {
        if program.emits.is_empty() {
            return;
        }
        let view = lowered.program();
        for template in program.emits {
            assert!(
                view.functions()
                    .any(|(_, function)| function.name == *template),
                "`{}` lists focus template `{template}`, which is not in the module catalog",
                program.name
            );
        }
        let mut matched = 0;
        for (id, instance) in view.instances() {
            let Some(function) = view.function(instance.template) else {
                continue;
            };
            if !program.emits.contains(&function.name.as_str()) || instance.body.is_none() {
                continue;
            }
            matched += 1;
            let name = view
                .planned_name(id)
                .expect("a materialized instance has a planned name");
            assert!(
                defined.contains(name),
                "`{}`: focus instance `{name}` ({}) was not defined",
                program.name,
                function.name
            );
        }
        assert!(
            matched > 0,
            "`{}` lists focus templates but the module has no materialized instance of them",
            program.name
        );
    }

    /// One generic fixture instantiates its pair or runner twice,
    /// with distinct owners, so the emitted module proves the second
    /// instantiation did not reuse the first artifact.
    fn assert_distinct_d5_artifacts(program: &CorpusProgram, lowered: &LoweredModule) {
        use crate::{LoweredArtifactPlan, ReactiveRunnerBody};
        let family = program
            .generic_artifacts
            .expect("a MustRun fixture names its generic family");
        let mut owners = Vec::new();
        for (_, artifact) in lowered.program().artifacts() {
            match (family, artifact.plan.as_ref()) {
                (
                    super::CorpusGenericArtifacts::CoroutinePairs,
                    Some(LoweredArtifactPlan::CoroutineCodes(plan)),
                ) => owners.push(format!("{:?}", plan.body)),
                (
                    super::CorpusGenericArtifacts::ReactionRunners,
                    Some(LoweredArtifactPlan::ReactionRunner(plan)),
                ) if matches!(plan.body, ReactiveRunnerBody::Reaction { .. }) => {
                    owners.push(format!("{:?}", plan.owner));
                }
                (
                    super::CorpusGenericArtifacts::UntilRunners,
                    Some(LoweredArtifactPlan::UntilRunner(plan)),
                ) if matches!(plan.body, ReactiveRunnerBody::Until { .. }) => {
                    owners.push(format!("{:?}", plan.owner));
                }
                (
                    super::CorpusGenericArtifacts::DerivedRunners,
                    Some(LoweredArtifactPlan::DerivedRunner(plan)),
                ) if matches!(plan.body, ReactiveRunnerBody::Derived { .. }) => {
                    owners.push(format!("{:?}", plan.owner));
                }
                _ => {}
            }
        }
        assert_eq!(
            owners.len(),
            2,
            "`{}`: the generic fixture emits two artifacts, got {owners:?}",
            program.name
        );
        assert_ne!(
            owners[0], owners[1],
            "`{}`: the two generic artifacts have distinct owners",
            program.name
        );
    }
}
