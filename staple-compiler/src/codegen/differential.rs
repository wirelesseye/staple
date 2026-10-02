//! Stage 5.3 Step 6: the differential-harness corpus and its in-process test.
//!
//! One ordered corpus list is shared by the in-process body comparison here
//! and the CLI behavior harness in `staple-cli`. Each entry is tagged with the
//! substage that added it; later substages only append.
//!
//! The in-process test emits every program with the legacy backend and with
//! the strict lowered emitter, verifies both modules, runs the Stage 5.3
//! declaration census, and compares the normalized body of every function the
//! lowered emitter emitted with its mapped legacy function.
//! Normalization renames symbols through the census map, renumbers SSA values
//! and block labels in order of appearance, and sorts
//! `__staple_gc_register_root` calls.

#[cfg(any(test, feature = "differential-shadow"))]
use crate::LoweredModule;
#[cfg(any(test, feature = "differential-shadow"))]
use crate::lower::census::CensusMapping;
#[cfg(any(test, feature = "differential-shadow"))]
use std::collections::{HashMap, HashSet};

/// Where one corpus program's source comes from.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DifferentialSource {
    /// Source text inline in the corpus.
    Inline(&'static str),
    /// A path relative to the workspace root.
    File(&'static str),
}

/// What the CLI harness requires of one corpus program.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DifferentialExpectation {
    /// The program declares a C symbol no library defines (the census
    /// programs' `inspect`), so it can never link or run under either
    /// emitter. The CLI harness only checks that both emitters compile it.
    CompileOnly,
    /// The program must compile strictly and behave exactly like legacy.
    MustRun,
    /// The D5 generic fixtures: legacy aliases or rejects the program, so only
    /// the lowered emitter is run and its pinned stdout is required. Legacy
    /// must fail to compile or behave differently under the test, which
    /// records which.
    LoweredOnly,
}

/// The D5 artifact family a `LoweredOnly` fixture must instantiate twice.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DifferentialD5 {
    /// A generic coroutine's `resume`/`cleanup` pair.
    CoroutinePairs,
    /// A generic reaction's runner.
    ReactionRunners,
    /// A generic `until`'s runner.
    UntilRunners,
    /// A generic derived binding's runner.
    DerivedRunners,
}

/// One differential corpus program.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct DifferentialProgram {
    /// Stable label used in reports and failure messages.
    pub name: &'static str,
    pub source: DifferentialSource,
    /// The substage that added the entry.
    pub substage: &'static str,
    /// What the CLI harness requires of the program.
    pub expectation: DifferentialExpectation,
    /// Stage 5.4 Step 1: template names from the program's own module
    /// (matched against `LoweredFunction::name`). Every instance of each
    /// listed template must be fully emitted by the lowered emitter and
    /// body-identical to legacy. This is the per-feature gate that does not
    /// need a runnable program.
    pub emits: &'static [&'static str],
    /// The exact stdout a `MustRun` program prints. The CLI harness asserts it
    /// under both emitters, so a defect mirrored by both cannot pass as parity.
    pub expected_stdout: Option<&'static str>,
    /// A `MustRun` program that must end in an `llvm.trap` (killed by a
    /// signal, so no exit code) under both emitters.
    pub traps: bool,
    /// A `LoweredOnly` program's D5 artifact family: the in-process harness
    /// requires two distinct artifacts of it.
    pub d5: Option<DifferentialD5>,
}

const fn inline(
    name: &'static str,
    source: &'static str,
    substage: &'static str,
) -> DifferentialProgram {
    DifferentialProgram {
        name,
        source: DifferentialSource::Inline(source),
        substage,
        expectation: DifferentialExpectation::MustRun,
        emits: &[],
        expected_stdout: None,
        traps: false,
        d5: None,
    }
}

const fn file(
    name: &'static str,
    path: &'static str,
    substage: &'static str,
) -> DifferentialProgram {
    DifferentialProgram {
        name,
        source: DifferentialSource::File(path),
        substage,
        expectation: DifferentialExpectation::MustRun,
        emits: &[],
        expected_stdout: None,
        traps: false,
        d5: None,
    }
}

/// Stage 5.8 Step 8: a D5 generic fixture only the lowered emitter runs.
const fn lowered_only(mut program: DifferentialProgram, d5: DifferentialD5) -> DifferentialProgram {
    program.expectation = DifferentialExpectation::LoweredOnly;
    program.d5 = Some(d5);
    program
}

/// Marks a corpus entry as unlinkable under either emitter.
const fn compile_only(mut program: DifferentialProgram) -> DifferentialProgram {
    program.expectation = DifferentialExpectation::CompileOnly;
    program
}

/// An entry that compiles strictly and runs identically under both emitters.
const fn must_run(mut program: DifferentialProgram) -> DifferentialProgram {
    program.expectation = DifferentialExpectation::MustRun;
    program
}

/// Requires a `MustRun` entry to end in a trap under both emitters.
const fn expect_trap(mut program: DifferentialProgram) -> DifferentialProgram {
    program.traps = true;
    program
}

/// Pins the exact stdout a `MustRun` entry prints under both emitters.
const fn expect_stdout(
    mut program: DifferentialProgram,
    stdout: &'static str,
) -> DifferentialProgram {
    program.expected_stdout = Some(stdout);
    program
}

/// Stage 5.4 Step 1: names the functions the entry must fully emit. Step 10
/// attaches the list to the 5.4 corpus entries.
#[allow(dead_code)]
const fn emits(
    mut program: DifferentialProgram,
    templates: &'static [&'static str],
) -> DifferentialProgram {
    program.emits = templates;
    program
}

/// Stage 5.3's differential corpus: the empty program, a non-generic
/// integer-arithmetic program, a two-module program with module globals and
/// initialization state, the Stage 4.7 census programs plus an
/// every-artifact-family fixture, `staple-compiler/examples/*.sta` (excluding
/// `macros.sta`, which fails during lowering), and the two-module `game_loop`
/// example.
#[doc(hidden)]
pub fn differential_corpus() -> &'static [DifferentialProgram] {
    &CORPUS
}

static CORPUS: [DifferentialProgram; 74] = [
    expect_stdout(must_run(inline("empty", "", "5.3")), ""),
    expect_stdout(
        must_run(inline(
            "integer_arithmetic",
            concat!(
                "def plus: (I32, I32) -> I32 = (left, right) => left + right\n",
                "let first = plus (1, 2)\n",
                "let second = first + 3\n",
            ),
            "5.3",
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
            "5.3",
        )),
        "",
    ),
    expect_stdout(
        must_run(inline(
            "census_constructor_adapters",
            concat!(
                "type Point = ctor (I32, I32)\n",
                "let make: () -> ((I32, I32) -> Point) = () => Point\n",
                "type Resource = ctor I32\n",
                "impl Drop Resource { def drop = Resource value => () }\n",
                "let make_resource: () -> (Resource -> Ref Resource) = () => Ref\n",
                "def ref_maker: <T where Copy T> () -> (T -> Ref T) = () => Ref\n",
                "let maker_i32: I32 -> Ref I32 = ref_maker ()\n",
                "let maker_u8: U8 -> Ref U8 = ref_maker ()\n",
            ),
            "5.3",
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
            "5.3",
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
        "5.3",
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
            "def peek: <T> T -> I32 = _ => 1\n",
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
        "5.3",
    )),
    compile_only(inline(
        "all_artifact_families",
        concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "use std.buffer.*\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "extern \"c\" { abs: I32 -> I32 }\n",
            "type Point = ctor (I32, I32)\n",
            "let make_point: () -> ((I32, I32) -> Point) = () => Point\n",
            "let pair = (1, 2)\n",
            "let shown = \"${pair:?}\"\n",
            "type Resource = ctor I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
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
        "5.3",
    )),
    expect_stdout(
        must_run(file(
            "example_c_interop",
            "staple-compiler/examples/c_interop.sta",
            "5.3",
        )),
        "hello, world!\nowned Staple String\nCString round trip\n",
    ),
    expect_stdout(
        must_run(file(
            "example_coroutines",
            "staple-compiler/examples/coroutines.sta",
            "5.3",
        )),
        "-- pump 1 --\nworker 4: start\nworker 9: start\npump 1: executed=4 ready=2\n-- pump 2 --\nworker 4: done\ngreeter: host sent 200\npump 2: executed=4 ready=1\n-- pump 3 --\nconsumer: worker produced 40\npump 3: executed=1 ready=0\ndoomed finished: True\nscope closed\n",
    ),
    expect_stdout(
        must_run(file(
            "example_hello_world",
            "staple-compiler/examples/hello_world.sta",
            "5.3",
        )),
        "Hello, world!\n",
    ),
    expect_stdout(
        must_run(file(
            "example_language_tour",
            "staple-compiler/examples/language_tour.sta",
            "5.3",
        )),
        "Staple language tour\nProducts, closures, nested patterns, and operators evaluated successfully.\n",
    ),
    expect_stdout(
        must_run(file(
            "example_list_and_collections",
            "staple-compiler/examples/list_and_collections.sta",
            "5.3",
        )),
        "8\n8\n0\n7\n128\n7\n7\n3\n",
    ),
    expect_stdout(
        must_run(file(
            "example_modules_and_imports",
            "staple-compiler/examples/modules_and_imports.sta",
            "5.3",
        )),
        "module imports\nNamespace, selected, renamed, and glob imports evaluated successfully.\n",
    ),
    expect_stdout(
        must_run(file(
            "example_signals_and_reactions",
            "staple-compiler/examples/signals_and_reactions.sta",
            "5.3",
        )),
        "count: 0, doubled: 0\ncount: 3, doubled: 6\ninitial snapshot: 0, current count: 3\n",
    ),
    expect_stdout(
        must_run(file(
            "example_sums_and_propagation",
            "staple-compiler/examples/sums_and_propagation.sta",
            "5.3",
        )),
        "found\nnot found\nExplicit return evaluated successfully.\n",
    ),
    expect_stdout(
        must_run(file(
            "example_traits_and_generics",
            "staple-compiler/examples/traits_and_generics.sta",
            "5.3",
        )),
        "integer\ntrue\n",
    ),
    expect_stdout(
        must_run(file(
            "example_types_and_matching",
            "staple-compiler/examples/types_and_matching.sta",
            "5.3",
        )),
        "Nominal patterns, generic constructors, and singleton matches evaluated successfully.\ndifferent\n",
    ),
    // A two-module program: `main.sta` resolves `use game.*` against its own
    // directory.
    expect_stdout(
        must_run(file(
            "example_game_loop",
            "staple-compiler/examples/game_loop/main.sta",
            "5.3",
        )),
        "fixed tick 1\nfixed tick 2\nfixed tick 3\nentity destroyed\nwall sleeper woke: wall_ms=300\nfinal: game_ms=100 wall_ms=400 frame=4 fixed=3\n",
    ),
    expect_stdout(
        must_run(
            // Stage 5.4: calls, callable values, closures, resources, and intrinsics.
            // Each entry names the functions its `emits` list must fully emit;
            // Stage 5.6 Step 8 flipped every one of them to `MustRun`.
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
                    "5.4",
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
                    "def sum3: (I32, b: I32 = 2, c: I32 = 3) -> I32 = (a, b, c) => a + b + c\n",
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
                "5.4",
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
                    "type MoveOnly = ctor I32\n",
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
                "5.4",
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
                    "type MoveOnly = ctor I32\n",
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
                "5.4",
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
                "5.4",
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
                    "type Point = ctor (I32, I32)\n",
                    "type Resource = ctor I32\n",
                    "impl Drop Resource { def drop = Resource value => () }\n",
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
                "5.4",
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
                    "pub type Counter = pub ctor (value: I32)\n",
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
                "5.4",
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
                "5.4",
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
                    "def measure: CString -> I32 = text => 1\n",
                    // The thunk captures an owned `CString`, so lowering records a
                    // `ThunkArgumentEnvironment` finalizer use. `thunk_env` owns
                    // its moved parameter; Stage 5.6 Step 4 emits its scope exit.
                    // It reads the capture through a Staple function: calling the
                    // `puts` extern value here would hit the legacy extern adapter
                    // ABI defect (D5, fixed in 5.11) and print a heap address.
                    "def thunk_env: move CString -> I32 = move value => evaluate { measure value }\n",
                    "let first = thunk_plain 1\n",
                    "let second = thunk_env (c_string \"thunk\\n\")\n",
                ),
                "5.4",
            ),
            &["evaluate", "thunk_plain", "measure", "thunk_env"],
        )),
        "",
    ),
    expect_stdout(
        must_run(emits(
            inline(
                "match_sums_products",
                concat!(
                    "type Ok T = ctor T\n",
                    "type IOError = ctor String\n",
                    "type Inner = ctor (I32, I32)\n",
                    "type Sign = ctor I32\n",
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
                "5.5",
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
                "5.5",
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
                    "pub type Pair = pub ctor (I32, I32)\n",
                    "type Outer = ctor (Pair, String)\n",
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
                "5.5",
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
                    "type Counter = ctor I32\n",
                    "type Wrapper = ctor (value: I32)\n",
                    "impl Index Counter String I32 { def index = (counter, key) => 0 }\n",
                    "impl MutateIndex Counter String I32 { def mutate_index = (mut counter, key, move value) => () }\n",
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
                "5.5",
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
                    "type Ok T = ctor T\n",
                    "type IOError = ctor String\n",
                    "type Other = ctor String\n",
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
                "5.5",
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
                "5.5",
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
                    "type Found T = ctor T\n",
                    "type Missing = ctor String\n",
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
                "5.5",
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
                "5.5",
            ),
            &["show", "debug", "both"],
        )),
        "",
    ),
    // Stage 5.6: ownership cleanup, finalizers, and buffers.
    must_run(expect_stdout(
        emits(
            inline(
                "drop_order",
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Tag = ctor CString\n",
                    "impl Drop Tag { def drop = Tag text => { puts text; () } }\n",
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
                    "type Ok = ctor I32\n",
                    "type Bad = ctor CString\n",
                    "impl Drop Bad { def drop = Bad text => { puts text; () } }\n",
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
                "5.6",
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
                    "type Handle = ctor CString\n",
                    "impl Drop Handle { def drop = Handle text => { puts text; () } }\n",
                    "type Wrapper = ctor Handle\n",
                    "type Left = ctor (CString, I32)\n",
                    "type Right = ctor (I32, I32)\n",
                    "type Empty = ctor ()\n",
                    "type Chain = ctor (I32, Ref (Empty | Chain))\n",
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
                "5.6",
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
                    "type Payload = ctor CString\n",
                    "impl Drop Payload { def drop = Payload value => () }\n",
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
                "5.6",
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
                    "type Tag = ctor CString\n",
                    "impl Drop Tag { def drop = Tag text => () }\n",
                    "impl Clone Tag { def clone = Tag text => Tag (c_string \"clone\") }\n",
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
                "5.6",
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
                "5.6",
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
                    "type Payload = ctor CString\n",
                    "\n",
                    "def replace: () -> () = () => {\n",
                    "    let mut reference: Ref Payload = Ref (Payload (c_string \"original\"))\n",
                    "    let replaced: Payload = Ref.replace reference (Payload (c_string \"replacement\"))\n",
                    "    ()\n",
                    "}\n",
                    "\n",
                    "replace ()\n",
                ),
                "5.6",
            ),
            &["replace"],
        )),
        "",
    ),
    // Stage 5.7: every structural body, formatting delegates, and cleanup.
    must_run(expect_stdout(
        emits(
            inline(
                "structural_debug",
                r#"use std.cinterop.(CString, c_string)
extern "c" { puts: CString -> I32 }
type Held T = ctor (T)
impl<T where Debug T> Debug (Held T) { def fmt = (Held value, mut formatter) => Debug.fmt (value, formatter) }
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
                "5.7",
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
type Row = ctor (I32, I32)
impl Index Row USize I32 { def index = (row, position) => 7 }
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
                "5.7",
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
                "5.7",
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
type Tag = ctor CString
impl Drop Tag { def drop = Tag text => { puts text; () } }
def replace_owned: move (Tag, Tag) -> () = move pair => {
    let mut own = pair
    own[0] = Tag (c_string "replacement")
    ()
}
replace_owned (Tag (c_string "old"), Tag (c_string "second"))
"#,
                "5.7",
            ),
            &["replace_owned"],
        ),
        "old\nsecond\nreplacement\n",
    )),
    // Stage 5.7 review: runtime coverage for a droppable element replaced
    // through a reference and for the structural bounds traps.
    must_run(expect_stdout(
        emits(
            inline(
                "structural_ref_mutation_drop",
                r#"use std.cinterop.(CString, c_string)
extern "c" { puts: CString -> I32 }
type Tag = ctor CString
impl Drop Tag { def drop = Tag text => { puts text; () } }
def ref_mutate: move (Ref (Tag, Tag)) -> () = move reference => {
    let mut own = reference
    own[0] = Tag (c_string "ref replacement")
    ()
}
ref_mutate (Ref (Tag (c_string "ref old"), Tag (c_string "ref second")))
"#,
                "5.7",
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
                "5.7",
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
                "5.7",
            ),
            &["at"],
        ))),
        "",
    ),
    // Stage 5.8 Step 8: the D5 generic fixtures. Legacy rejects each of these
    // (an unspecialized type parameter reaches its emitter), so only the
    // lowered emitter runs them; each instantiates its artifact twice and
    // prints proof that the second instantiation used its own pair or runner.
    lowered_only(
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
                "5.8",
            ),
            "a=7 b=1\n",
        ),
        DifferentialD5::CoroutinePairs,
    ),
    // Stage 5.8 review: the D5 case legacy compiles but gets wrong. The
    // result type is concrete while the capture is `T`, so legacy's
    // syntax-keyed cache reuses the `I32` pair for the `(U8, U8)` creation
    // and prints `513` (the bytes `01 02` read as an `I32`). The lowered
    // emitter runs each instantiation's own pair.
    lowered_only(
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
                "5.8",
            ),
            "7\n(1, 2)\n",
        ),
        DifferentialD5::CoroutinePairs,
    ),
    lowered_only(
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
                "5.8",
            ),
            "reaction 7\nreaction 1\n",
        ),
        DifferentialD5::ReactionRunners,
    ),
    lowered_only(
        expect_stdout(
            inline(
                "generic_until_runner",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "let signal count = 0\n",
                    "def peek: <T> T -> I32 = _ => 0\n",
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
                "5.8",
            ),
            "until 7\nuntil 1\n",
        ),
        DifferentialD5::UntilRunners,
    ),
    lowered_only(
        expect_stdout(
            inline(
                "generic_derived_runner",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.io.(IO, println)\n",
                    "let signal count = 0\n",
                    "def peek: <T> T -> I32 = _ => 0\n",
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
                "5.8",
            ),
            "derived 7 0\nderived 1 0\nresults 0 0\n",
        ),
        DifferentialD5::DerivedRunners,
    ),
    // Stage 5.8 Step 9: the 4.5 fixture set as runnable programs, plus the
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
                "5.8",
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
                "5.8",
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
                    // Stage 5.8 review: cancel coroutines parked on an
                    // unresolved `Wait` (the unwind abandons the record) and
                    // on an `until` child (the unwind runs the child's
                    // cleanup); neither body resumes.
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
                "5.8",
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
                "5.8",
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
                    "type Counter = ctor (n: I32)\n",
                    "def read: () ->{Counter} I32 = () => (resource Counter).n\n",
                    "def subscribe: () ->{Reactive, Counter, IO} () = () => reaction { println \"count ${read ():?}\"; () }\n",
                    "with Counter = Counter (n: 5) {\n",
                    "  with Reactive = reactive_scope () {\n",
                    "    subscribe ()\n",
                    "  }\n",
                    "}\n",
                ),
                "5.8",
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
                "5.8",
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
                    "let signal base = 0\n",
                    "def render: (CString, I32) -> String = (text, extra) => CString.to_string text\n",
                    "def make: move CString ->{state.read, IO} String = move text => {\n",
                    "  let derived = render (text, base)\n",
                    "  println \"derived ${derived}\"\n",
                    "  derived\n",
                    "}\n",
                    "base = 3\n",
                    "let value = make (c_string \"len\")\n",
                    "println \"value ${value}\"\n",
                ),
                "5.8",
            ),
            &["render", "make"],
        ),
        "derived len\nvalue len\n",
    )),
    // K2/D5: the cancel unwind drops the frame bindings in plan order
    // (`first` then `second`); the completed sibling's `kept` frame binding is
    // never dropped — the mirrored completed-coroutine leak (D5).
    must_run(expect_stdout(
        emits(
            inline(
                "coroutine_drop_order",
                concat!(
                    "use std.coroutine.*\n",
                    "use std.cinterop.(CString, c_string)\n",
                    "use std.io.(IO, println)\n",
                    "extern \"c\" { puts: CString -> I32 }\n",
                    "type Tag = ctor CString\n",
                    "impl Drop Tag { def drop = Tag text => { puts text; () } }\n",
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
                "5.8",
            ),
            &["tag", "with_tags", "completing"],
        ),
        "tags ready\ncompleting\nfirst\nsecond\ncancelled True\nscope closed\n",
    )),
    must_run(expect_stdout(
        inline(
            "slice_ref_coercion",
            "use std.slice.Slice\nlet fixed: Ref (I32; 3) = Ref (1, 2, 3)\nlet values: Slice I32 = fixed\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "local_recursive_cells",
            "def outer: () -> I32 = () => {\n def f: () -> I32 = () => g ()\n def g: () -> I32 = () => f ()\n f ()\n}\n",
            "5.9",
        ),
        "",
    )),
    // Step 5 shadow findings: a stored-closure callee builds legacy's unused
    // state slot, a captured `mut` field write checks its base without
    // initializing it, and an await/until body's effect-less legacy pair maps
    // to its effect-specialized instance.
    must_run(expect_stdout(
        inline(
            "local_recursive_call_state_slot",
            "def outer = value: I32 => {\n  def recurse: I32 -> I32 = n => {\n    let captured = value\n    recurse n\n  }\n  recurse 1\n}\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "captured_mut_field_write",
            "def make = () => {\n  let mut point = (x: 1, y: 2)\n  let update = () => { point.x = point.x + 1; point.x }\n  update ()\n}\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "await_task_effect_pair",
            "use std.coroutine.*\nuse std.io.(IO, println)\ndef leaf: () -> Coroutine{} I32 = () => coro { 9 }\ndef waiter: () -> Coroutine{Tasks, IO} I32 = () => coro {\n    let t = spawn (leaf ())\n    let r = await t\n    match r {\n        Completed v => v,\n        Cancelled() => 0,\n    }\n}\nlet sched = scheduler ()\nwith Tasks = task_scope (sched) {\n    let _ = spawn (waiter ())\n    let _ = pump (sched, 8)\n}\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "local_generic_cells",
            "def outer: () -> I32 = () => {\n def recur: <T> T -> T = value => recur value\n recur 1\n}\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "natural_repeated_return",
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\nlet repeated: (I32; 3) = repeat 7 3\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "effect_closure_resources",
            "use std.io.(IO, println)\ndef twice: <effect E> (() ->{E} ()) ->{E} () = f => { f (); f () }\ndef output: () ->{IO} () = () => println \"hello\"\ntwice output\n",
            "5.9",
        ),
        "hello\nhello\n",
    )),
    must_run(expect_stdout(
        inline(
            "product_trait_argument",
            "trait Merge Left Right Output { merge: (Left, Right) -> Output }\nimpl Merge I32 I32 I32 { def merge = (left, right) => left + right }\ndef combine: <L, R, O where Merge L R O> (L, R) -> O = pair => Merge.merge pair\nlet total: I32 = combine (20, 22)\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "nested_expression_return",
            "def identity = (value: I32) => value\ndef answer = () => { identity { return 42; }; 0; }\nanswer ()\n",
            "5.9",
        ),
        "",
    )),
    must_run(expect_stdout(
        inline(
            "sibling_initializer_names",
            "let a: I32 = { mod foo { pub let value: I32 = 1 }; foo.value }\nlet b: I32 = { mod foo { pub let value: I32 = 2 }; foo.value }\n",
            "5.9",
        ),
        "",
    )),
    expect_stdout(
        inline(
            "block_tail_coroutine",
            "use std.coroutine.*\ndef f: () -> Coroutine{} I32 = () => { coro { 42 } }\nlet held = f ()\n",
            "5.9",
        ),
        "",
    ),
];

/// Extract every `define`d function body from one module's IR text, keyed by
/// the function's final symbol name.
#[cfg(any(test, feature = "differential-shadow"))]
fn module_functions(ir: &str) -> HashMap<String, Vec<String>> {
    let lines = ir.lines().collect::<Vec<_>>();
    let mut functions = HashMap::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("define ") && !line.ends_with('}') {
            let after_at = line
                .find('@')
                .unwrap_or_else(|| panic!("define names a symbol: {line}"));
            let rest = &line[after_at + 1..];
            let end = rest
                .find(|character: char| character == '(' || character == ' ')
                .unwrap_or(rest.len());
            let mut body = vec![line.to_string()];
            index += 1;
            while index < lines.len() {
                body.push(lines[index].to_string());
                if lines[index] == "}" {
                    break;
                }
                index += 1;
            }
            functions.insert(rest[..end].to_string(), body);
        }
        index += 1;
    }
    functions
}

/// Every module-level constant (`@name = ... constant ...`), keyed by name,
/// with its definition after `=`. LLVM numbers constants such as
/// `@c_string.literal.3` in creation order, which differs between the two
/// emitters, so a body comparison must compare what a referenced constant
/// holds, never its name.
#[cfg(any(test, feature = "differential-shadow"))]
fn module_constants(ir: &str) -> HashMap<String, String> {
    let mut constants = HashMap::new();
    for line in ir.lines() {
        let Some(rest) = line.strip_prefix('@') else {
            continue;
        };
        let Some((name, definition)) = rest.split_once(" = ") else {
            continue;
        };
        if definition.contains(" constant ") || definition.starts_with("constant ") {
            constants.insert(name.trim_matches('"').to_owned(), definition.to_owned());
        }
    }
    constants
}

#[cfg(any(test, feature = "differential-shadow"))]
fn identifier_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '$' | '.' | '_' | '-')
}

/// Normalize one function body for comparison: rename `@` symbols through
/// `renames`, replace a reference to a module constant with that constant's
/// definition from `constants` (the body's own module), canonicalize `%`
/// locals and block labels in order of first appearance, and sort
/// `__staple_gc_register_root` calls to the end.
#[cfg(any(test, feature = "differential-shadow"))]
fn normalize_function(
    lines: &[String],
    renames: &HashMap<String, String>,
    constants: &HashMap<String, String>,
) -> Vec<String> {
    let mut locals: HashMap<String, String> = HashMap::new();
    let mut body = Vec::new();
    let mut roots = Vec::new();
    for line in lines {
        let mut output = String::new();
        let mut chars = line.chars().peekable();
        // A label definition starts the line at column zero.
        let mut label = String::new();
        let mut lookahead = line.chars();
        while let Some(character) = lookahead.next() {
            if identifier_char(character) {
                label.push(character);
            } else {
                break;
            }
        }
        let is_label = !label.is_empty() && line[label.len()..].starts_with(':');
        if is_label {
            output.push_str(&canonical_name(&mut locals, &label));
            for _ in 0..label.len() {
                chars.next();
            }
        }
        while let Some(character) = chars.next() {
            if character == '%' || character == '@' {
                let mut token = String::new();
                while let Some(next) = chars.peek().copied() {
                    if identifier_char(next) {
                        token.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if character == '%' {
                    output.push('%');
                    output.push_str(&canonical_name(&mut locals, &token));
                } else {
                    output.push('@');
                    output.push_str(&canonical_symbol(&token, renames, constants));
                }
            } else {
                output.push(character);
            }
        }
        // LLVM pads a block label's `; preds = ...` comment to the widest line
        // in the function, and the two emitters' pre-normalization names have
        // different lengths. The padding is not part of the body.
        if is_label && let Some(index) = output.find(';') {
            let (head, tail) = output.split_at(index);
            output = format!("{} {}", head.trim_end(), tail);
        }
        if output.contains("call void @__staple_gc_register_root") {
            roots.push(output);
        } else {
            body.push(output);
        }
    }
    roots.sort();
    body.extend(roots);
    body
}

#[cfg(any(test, feature = "differential-shadow"))]
/// The comparison name of one `@` symbol: its planned name through the
/// census map when it is a mapped function, otherwise its base name with the
/// internal disambiguator removed. Decision D2 lets the legacy backend
/// disambiguate a module global with LLVM's `.N` suffix while the lowered
/// backend uses `unique_global_name`'s `.global.<symbol>`; both denote the
/// same catalog symbol.
#[cfg(any(test, feature = "differential-shadow"))]
fn canonical_symbol(
    token: &str,
    renames: &HashMap<String, String>,
    constants: &HashMap<String, String>,
) -> String {
    if let Some(planned) = renames.get(token) {
        return planned.clone();
    }
    // A constant compares by content: two emitters number the same literal
    // differently, and a stripped `.N` alone would equate different literals.
    if let Some(definition) = constants.get(token) {
        return format!("constant{{{definition}}}");
    }
    if let Some(index) = token.rfind(".global.")
        && token[index + ".global.".len()..]
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return token[..index].to_owned();
    }
    if let Some(index) = token.rfind('.')
        && !token[index + 1..].is_empty()
        && token[index + 1..]
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return token[..index].to_owned();
    }
    token.to_owned()
}

#[cfg(any(test, feature = "differential-shadow"))]
fn canonical_name(locals: &mut HashMap<String, String>, token: &str) -> String {
    if let Some(name) = locals.get(token) {
        return name.clone();
    }
    let name = format!("v{}", locals.len());
    locals.insert(token.to_owned(), name.clone());
    name
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use inkwell::context::Context;

    use crate::lower::census::assert_declaration_parity;
    use crate::{LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker};

    use super::{
        DifferentialProgram, DifferentialSource, compare_fully_emitted_bodies, differential_corpus,
        module_constants, normalize_function,
    };

    fn workspace_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent")
    }

    /// Freeze CLI inputs for the Stage 5.10 IR identity gate.
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

        for program in differential_corpus() {
            let directory = destination.join(program.name);
            std::fs::create_dir_all(&directory).unwrap();
            match program.source {
                DifferentialSource::Inline(source) => {
                    std::fs::write(directory.join("main.sta"), source).unwrap();
                }
                DifferentialSource::File(path) => {
                    let source = workspace_root().join(path);
                    copy_sources(source.parent().unwrap(), &directory);
                    std::fs::copy(source, directory.join("main.sta")).unwrap();
                }
            }
        }
        eprintln!("dumped {} corpus programs", differential_corpus().len());
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
    fn program_source(program: &DifferentialProgram) -> (String, std::path::PathBuf) {
        match program.source {
            DifferentialSource::Inline(source) => {
                (source.to_owned(), workspace_root().to_path_buf())
            }
            DifferentialSource::File(path) => {
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

    /// A constant reference compares by the constant's content: the same
    /// literal under different LLVM numbering matches, a different literal
    /// under the same stripped name does not.
    #[test]
    fn normalization_compares_constants_by_content() {
        let body = |constant: &str| {
            vec![
                "define void @f() {".to_owned(),
                format!("  %0 = call ptr @use(ptr @{constant})"),
                "  ret void".to_owned(),
                "}".to_owned(),
            ]
        };
        let legacy = module_constants(concat!(
            "@c_string.literal = private unnamed_addr constant [3 x i8] c\"hi\\00\", align 1\n",
            "@c_string.literal.1 = private unnamed_addr constant [3 x i8] c\"no\\00\", align 1\n",
        ));
        let lowered = module_constants(concat!(
            "@c_string.literal.4 = private unnamed_addr constant [3 x i8] c\"hi\\00\", align 1\n",
            "@c_string.literal.5 = private unnamed_addr constant [3 x i8] c\"xx\\00\", align 1\n",
        ));
        let renames = HashMap::new();
        assert_eq!(
            normalize_function(&body("c_string.literal"), &renames, &legacy),
            normalize_function(&body("c_string.literal.4"), &renames, &lowered),
            "the same literal under different numbering matches"
        );
        assert_ne!(
            normalize_function(&body("c_string.literal.1"), &renames, &legacy),
            normalize_function(&body("c_string.literal.5"), &renames, &lowered),
            "a different literal must not match after `.N` stripping"
        );
    }

    /// Over the whole corpus, emit both modules strictly, run the
    /// declaration census, compare every function's normalized body with its
    /// mapped legacy function, and check each entry's focus `emits`
    /// templates.
    #[test]
    fn stage_5_3_differential_harness_reports_and_matches_bodies() {
        let mut structural_kinds = std::collections::HashSet::new();
        let mut structural_bodies = std::collections::HashSet::new();
        let mut compared = 0;
        // The frozen oracle: every runnable entry pins the stdout (and, for a
        // trap, the missing exit status) legacy produced, so the behavior
        // check survives the deletion of the legacy emitter.
        for program in differential_corpus() {
            if matches!(
                program.expectation,
                super::DifferentialExpectation::MustRun
                    | super::DifferentialExpectation::LoweredOnly
            ) {
                assert!(
                    program.expected_stdout.is_some(),
                    "runnable corpus entry `{}` does not pin its output",
                    program.name
                );
            }
        }
        for program in differential_corpus() {
            let (source, root) = program_source(program);
            let lowered = lower(&source, &root);
            if program.substage == "5.7" {
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
            let legacy = crate::codegen::legacy_emissions(&context, &lowered);
            let emitted = crate::codegen::lowered_emissions(&context, &lowered).unwrap_or_else(
                |diagnostics| {
                    panic!(
                        "strict lowered emission should succeed for `{}`: {diagnostics:?}\n{source}",
                        program.name
                    )
                },
            );
            let lowered_only = program.expectation == super::DifferentialExpectation::LoweredOnly;
            if lowered_only {
                // Legacy rejects or aliases these programs, so only the
                // lowered module's structure is asserted. If legacy did
                // compile one, the second artifact must be census-explained.
                assert_distinct_d5_artifacts(program, &lowered);
                crate::lower::census::assert_catalog_census(program.name, &lowered, &emitted);
                if let Ok(legacy) = legacy {
                    let mapping =
                        assert_declaration_parity(program.name, &lowered, &legacy, &emitted);
                    assert!(
                        !mapping.aliased_artifacts.is_empty(),
                        "`{}`: legacy compiled a LoweredOnly fixture without aliasing an artifact",
                        program.name
                    );
                }
            } else {
                let legacy = legacy.unwrap_or_else(|diagnostics| {
                    panic!(
                        "the legacy backend should compile `{}`: {diagnostics:?}\n{source}",
                        program.name
                    )
                });
                let mapping = assert_declaration_parity(program.name, &lowered, &legacy, &emitted);
                let compared_names = compare_fully_emitted_bodies(
                    program.name,
                    &lowered,
                    &mapping,
                    &legacy,
                    &emitted,
                );
                assert_focus_emissions(program, &lowered, &compared_names);
                compared += compared_names.len();
            }
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
                "5.7 corpus misses {kind:?}"
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
            assert!(structural_bodies.contains(body), "5.7 corpus misses {body}");
        }

        eprintln!("differential corpus: {compared} bodies compared");
        assert!(
            compared > 0,
            "the corpus must have emitted functions to compare"
        );
    }

    /// The legacy-free catalog census rejects an unplanned function, a
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

    /// Stage 5.4 Step 1: every instance of an entry's `emits` templates is
    /// fully emitted and body-compared. A template must exist in the module
    /// catalog and have at least one materialized instance.
    fn assert_focus_emissions(
        program: &DifferentialProgram,
        lowered: &LoweredModule,
        compared: &HashSet<String>,
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
                compared.contains(name),
                "`{}`: focus instance `{name}` ({}) was not body-compared against legacy",
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

    /// Stage 5.8 Step 8: one D5 fixture instantiates its pair or runner twice,
    /// with distinct owners, so the emitted module proves the second
    /// instantiation did not reuse the first artifact.
    fn assert_distinct_d5_artifacts(program: &DifferentialProgram, lowered: &LoweredModule) {
        use crate::{LoweredArtifactPlan, ReactiveRunnerBody};
        let family = program
            .d5
            .expect("a LoweredOnly fixture names its D5 family");
        let mut owners = Vec::new();
        for (_, artifact) in lowered.program().artifacts() {
            match (family, artifact.plan.as_ref()) {
                (
                    super::DifferentialD5::CoroutinePairs,
                    Some(LoweredArtifactPlan::CoroutineCodes(plan)),
                ) => owners.push(format!("{:?}", plan.body)),
                (
                    super::DifferentialD5::ReactionRunners,
                    Some(LoweredArtifactPlan::ReactionRunner(plan)),
                ) if matches!(plan.body, ReactiveRunnerBody::Reaction { .. }) => {
                    owners.push(format!("{:?}", plan.owner));
                }
                (
                    super::DifferentialD5::UntilRunners,
                    Some(LoweredArtifactPlan::UntilRunner(plan)),
                ) if matches!(plan.body, ReactiveRunnerBody::Until { .. }) => {
                    owners.push(format!("{:?}", plan.owner));
                }
                (
                    super::DifferentialD5::DerivedRunners,
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
            "`{}`: the D5 fixture emits two artifacts, got {owners:?}",
            program.name
        );
        assert_ne!(
            owners[0], owners[1],
            "`{}`: the two D5 artifacts have distinct owners",
            program.name
        );
    }
}

#[cfg(any(test, feature = "differential-shadow"))]
/// Compare every mapped function the lowered emitter emitted with its
/// legacy function. Returns the planned names compared.
pub(crate) fn compare_fully_emitted_bodies(
    label: &str,
    lowered: &LoweredModule,
    mapping: &CensusMapping,
    legacy: &crate::codegen::LegacyEmissions,
    emitted: &crate::codegen::LoweredEmissions,
) -> HashSet<String> {
    use crate::lower::census::planned_names_for;

    let program = lowered.program();
    // Legacy name -> canonical planned name, covering the specialization
    // duplicates the census collapses.
    let mut renames = HashMap::new();
    for (legacy_name, entry) in &mapping.symbol_names {
        let names = planned_names_for(program, entry);
        if let [planned] = names.as_slice() {
            renames.insert(legacy_name.clone(), planned.clone());
        }
    }
    // Planned names can also be legacy names for a different catalog
    // entry (sibling module initializers). Never apply the legacy map to
    // lowered symbols; preserve all catalog spellings on that side.
    let mut lowered_renames = HashMap::new();
    for (_, instance) in program.instances() {
        lowered_renames.insert(instance.name.clone(), instance.name.clone());
    }
    for (_, artifact) in program.artifacts() {
        lowered_renames.insert(artifact.name.clone(), artifact.name.clone());
        if let Some(crate::LoweredArtifactPlan::CoroutineCodes(_)) = artifact.plan {
            let (resume, cleanup) = program
                .planned_coroutine_pair_names(artifact.ordinal)
                .expect("pair has names");
            lowered_renames.insert(resume.clone(), resume);
            lowered_renames.insert(cleanup.clone(), cleanup);
        }
    }
    for (_, initializer) in program.initializers() {
        lowered_renames.insert(initializer.name.clone(), initializer.name.clone());
    }
    let legacy_functions = module_functions(&legacy.module_ir);
    let lowered_functions = module_functions(&emitted.module_ir);
    let legacy_constants = module_constants(&legacy.module_ir);
    let lowered_constants = module_constants(&emitted.module_ir);

    let mut compared = HashSet::new();
    for (legacy_name, entry) in &mapping.mapped {
        for planned in planned_names_for(program, entry) {
            let legacy_body = legacy_functions
                .get(legacy_name)
                .unwrap_or_else(|| panic!("legacy `{legacy_name}` has no body ({label})"));
            let lowered_body = lowered_functions.get(&planned).unwrap_or_else(|| {
                panic!("the lowered module has no body for `{planned}` ({label})")
            });
            // D5: a creation in a different concrete owner uses its own
            // pair, while legacy's syntax cache keeps the first pair.
            // Normalize only the census-proven CoroCreation references;
            // compare every other instruction in this owner's body.
            use crate::lower::census::LegacyCatalogEntry;
            let owner = match entry {
                LegacyCatalogEntry::Instance(id) => {
                    Some(crate::lower::EmissionOwner::Instance(*id))
                }
                LegacyCatalogEntry::Artifact { ordinal, .. } => program
                    .artifacts()
                    .find(|(_, artifact)| artifact.ordinal == *ordinal)
                    .and_then(|(_, artifact)| match &artifact.plan {
                        Some(crate::LoweredArtifactPlan::CoroutineCodes(pair)) => {
                            Some(crate::lower::EmissionOwner::Instance(pair.body))
                        }
                        _ => None,
                    }),
                LegacyCatalogEntry::Initializer(module) => program
                    .initializers()
                    .find(|(_, initializer)| initializer.module == *module)
                    .map(|(id, _)| crate::lower::EmissionOwner::Initializer(id)),
                _ => None,
            };
            let mut expected_renames = std::borrow::Cow::Borrowed(&renames);
            if let Some(owner) = owner {
                for use_ in program.artifact_uses(owner).unwrap_or(&[]) {
                    if !matches!(use_.site, crate::ArtifactUseSite::CoroCreation(_))
                        || !mapping.aliased_artifacts.contains(&use_.artifact)
                    {
                        continue;
                    }
                    let alias = program
                        .artifacts()
                        .find(|(_, artifact)| artifact.ordinal == use_.artifact)
                        .and_then(|(_, artifact)| artifact.plan.as_ref());
                    let Some(crate::LoweredArtifactPlan::CoroutineCodes(alias)) = alias else {
                        continue;
                    };
                    let alias_template = program
                        .instance(alias.body)
                        .expect("aliased pair body")
                        .template;
                    let mut found = 0;
                    for (name, target) in &mapping.symbol_names {
                        let LegacyCatalogEntry::Artifact { ordinal, slot } = target else {
                            continue;
                        };
                        let Some(crate::LoweredArtifactPlan::CoroutineCodes(pair)) = program
                            .artifacts()
                            .find(|(_, artifact)| artifact.ordinal == *ordinal)
                            .and_then(|(_, artifact)| artifact.plan.as_ref())
                        else {
                            continue;
                        };
                        if program
                            .instance(pair.body)
                            .expect("mapped pair body")
                            .template
                            != alias_template
                        {
                            continue;
                        }
                        let names = planned_names_for(
                            program,
                            &LegacyCatalogEntry::Artifact {
                                ordinal: use_.artifact,
                                slot: *slot,
                            },
                        );
                        let [target] = names.as_slice() else {
                            panic!("aliased pair slot has exactly one planned name");
                        };
                        expected_renames
                            .to_mut()
                            .insert(name.clone(), target.clone());
                        found += 1;
                    }
                    assert_eq!(
                        found, 2,
                        "census-confirmed alias has one legacy pair ({label})"
                    );
                }
            }
            let expected = normalize_function(legacy_body, &expected_renames, &legacy_constants);
            let actual = normalize_function(lowered_body, &lowered_renames, &lowered_constants);
            assert_eq!(
                actual, expected,
                "normalized body differs for legacy `{legacy_name}` -> `{planned}` ({label})"
            );
            compared.insert(planned);
        }
    }
    compared
}

/// Number of successful shadow program/body comparisons in this process.
#[doc(hidden)]
#[cfg(feature = "differential-shadow")]
pub fn shadow_counts() -> (usize, usize) {
    use std::sync::atomic::Ordering;
    (
        SHADOW_PROGRAMS.load(Ordering::Relaxed),
        SHADOW_BODIES.load(Ordering::Relaxed),
    )
}

#[cfg(feature = "differential-shadow")]
static SHADOW_PROGRAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(feature = "differential-shadow")]
static SHADOW_BODIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Number of programs legacy rejected under D5 (unspecialized type parameter).
#[doc(hidden)]
#[cfg(feature = "differential-shadow")]
pub fn shadow_rejections() -> usize {
    SHADOW_REJECTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(feature = "differential-shadow")]
static SHADOW_REJECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "differential-shadow")]
pub(crate) fn record_shadow_rejection() {
    use std::io::Write;
    SHADOW_REJECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(path) = std::env::var_os("STAPLE_DIFFERENTIAL_SHADOW_REPORT") {
        let mut report = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open shadow comparison ledger");
        report
            .write_all(format!("{}\treject\n", std::process::id()).as_bytes())
            .expect("append shadow comparison ledger");
    }
}

/// Optional append-only run ledger aggregates isolated nextest and CLI processes.
#[cfg(feature = "differential-shadow")]
pub(crate) fn record_shadow(bodies: usize) {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    SHADOW_PROGRAMS.fetch_add(1, Ordering::Relaxed);
    SHADOW_BODIES.fetch_add(bodies, Ordering::Relaxed);
    if let Some(path) = std::env::var_os("STAPLE_DIFFERENTIAL_SHADOW_REPORT") {
        let mut report = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open shadow comparison ledger");
        report
            .write_all(format!("{}\t{bodies}\n", std::process::id()).as_bytes())
            .expect("append shadow comparison ledger");
    }
}
