//! Stage 5.3 Step 6: the differential-harness corpus and its in-process test.
//!
//! One ordered corpus list is shared by the in-process body comparison here
//! and the CLI behavior harness in `staple-cli`. Each entry is tagged with the
//! substage that added it; later substages only append.
//!
//! The in-process test emits every program with the legacy backend and with
//! the lowered partial mode, verifies both modules, runs the Stage 5.3
//! declaration census, and compares the normalized body of every function the
//! lowered emitter fully emitted (no stub) with its mapped legacy function.
//! Normalization renames symbols through the census map, renumbers SSA values
//! and block labels in order of appearance, and sorts
//! `__staple_gc_register_root` calls.

#[cfg(test)]
use std::collections::HashMap;

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
    /// emitter. The CLI harness only checks that both emitters compile it
    /// (or that the lowered emitter reports it blocked).
    CompileOnly,
    /// The lowered emitter may still report the program blocked (strict
    /// emission fails); once it compiles, it must behave exactly like legacy.
    MayBeBlocked,
    /// The program must compile strictly and behave exactly like legacy. A
    /// substage flips an entry to this once the program first runs, so it can
    /// never regress to blocked (the ratchet the 5.6 runnable gate relies on).
    MustRun,
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
        expectation: DifferentialExpectation::MayBeBlocked,
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
        expectation: DifferentialExpectation::MayBeBlocked,
    }
}

/// Marks a corpus entry as unlinkable under either emitter.
const fn compile_only(mut program: DifferentialProgram) -> DifferentialProgram {
    program.expectation = DifferentialExpectation::CompileOnly;
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

static CORPUS: [DifferentialProgram; 19] = [
    inline("empty", "", "5.3"),
    inline(
        "integer_arithmetic",
        concat!(
            "def plus: (I32, I32) -> I32 = (left, right) => left + right\n",
            "let first = plus (1, 2)\n",
            "let second = first + 3\n",
        ),
        "5.3",
    ),
    inline(
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
    ),
    inline(
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
    ),
    inline(
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
    file(
        "example_c_interop",
        "staple-compiler/examples/c_interop.sta",
        "5.3",
    ),
    file(
        "example_coroutines",
        "staple-compiler/examples/coroutines.sta",
        "5.3",
    ),
    file(
        "example_hello_world",
        "staple-compiler/examples/hello_world.sta",
        "5.3",
    ),
    file(
        "example_language_tour",
        "staple-compiler/examples/language_tour.sta",
        "5.3",
    ),
    file(
        "example_list_and_collections",
        "staple-compiler/examples/list_and_collections.sta",
        "5.3",
    ),
    file(
        "example_modules_and_imports",
        "staple-compiler/examples/modules_and_imports.sta",
        "5.3",
    ),
    file(
        "example_signals_and_reactions",
        "staple-compiler/examples/signals_and_reactions.sta",
        "5.3",
    ),
    file(
        "example_sums_and_propagation",
        "staple-compiler/examples/sums_and_propagation.sta",
        "5.3",
    ),
    file(
        "example_traits_and_generics",
        "staple-compiler/examples/traits_and_generics.sta",
        "5.3",
    ),
    file(
        "example_types_and_matching",
        "staple-compiler/examples/types_and_matching.sta",
        "5.3",
    ),
    // A two-module program: `main.sta` resolves `use game.*` against its own
    // directory.
    file(
        "example_game_loop",
        "staple-compiler/examples/game_loop/main.sta",
        "5.3",
    ),
];

/// Extract every `define`d function body from one module's IR text, keyed by
/// the function's final symbol name.
#[cfg(test)]
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
#[cfg(test)]
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

#[cfg(test)]
fn identifier_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '$' | '.' | '_' | '-')
}

/// Normalize one function body for comparison: rename `@` symbols through
/// `renames`, replace a reference to a module constant with that constant's
/// definition from `constants` (the body's own module), canonicalize `%`
/// locals and block labels in order of first appearance, and sort
/// `__staple_gc_register_root` calls to the end.
#[cfg(test)]
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

#[cfg(test)]
/// The comparison name of one `@` symbol: its planned name through the
/// census map when it is a mapped function, otherwise its base name with the
/// internal disambiguator removed. Decision D2 lets the legacy backend
/// disambiguate a module global with LLVM's `.N` suffix while the lowered
/// backend uses `unique_global_name`'s `.global.<symbol>`; both denote the
/// same catalog symbol.
#[cfg(test)]
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

#[cfg(test)]
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

    use crate::lower::graph_validation::tests::{CensusMapping, assert_declaration_parity};
    use crate::{LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker};

    use super::{
        DifferentialProgram, DifferentialSource, differential_corpus, module_constants,
        module_functions, normalize_function,
    };

    fn workspace_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent")
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

    /// Stage 5.3 Step 6: over the whole corpus, verify both emitted modules,
    /// run the declaration census, compare every fully emitted function's
    /// normalized body with its mapped legacy function, and print the
    /// partial-mode report.
    #[test]
    fn stage_5_3_differential_harness_reports_and_matches_bodies() {
        let mut total_stubs = 0;
        let mut compared = 0;
        let mut histogram: HashMap<String, usize> = HashMap::new();
        for program in differential_corpus() {
            let (source, root) = program_source(program);
            let lowered = lower(&source, &root);
            let context = Context::create();
            let legacy = crate::codegen::legacy_emissions(&context, &lowered).unwrap_or_else(
                |diagnostics| {
                    panic!(
                        "the legacy backend should compile `{}`: {diagnostics:?}\n{source}",
                        program.name
                    )
                },
            );
            let partial = crate::codegen::lowered_partial_emissions(&context, &lowered)
                .unwrap_or_else(|diagnostics| {
                    panic!(
                        "partial lowered emission should verify `{}`: {diagnostics:?}\n{source}",
                        program.name
                    )
                });
            let mapping = assert_declaration_parity(program.name, &lowered, &legacy, &partial);

            total_stubs += partial.report.stubbed().len();
            for (family, count) in partial.report.family_histogram() {
                *histogram.entry(family.clone()).or_insert(0) += count;
            }
            compared +=
                compare_fully_emitted_bodies(program.name, &lowered, &mapping, &legacy, &partial);
        }

        let mut families = histogram.into_iter().collect::<Vec<_>>();
        families.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        eprintln!(
            "differential corpus: {compared} fully emitted bodies compared, {total_stubs} stubs across {} families",
            families.len()
        );
        for (family, count) in families.iter().take(12) {
            eprintln!("{count:5}  {family}");
        }
        assert!(
            compared > 0,
            "the corpus must have fully emitted functions to compare"
        );
    }

    /// Compare every function the lowered emitter fully emitted (not a stub)
    /// with its mapped legacy function. Returns the number compared.
    fn compare_fully_emitted_bodies(
        label: &str,
        lowered: &LoweredModule,
        mapping: &CensusMapping,
        legacy: &crate::codegen::LegacyEmissions,
        partial: &crate::codegen::LoweredPartialEmissions,
    ) -> usize {
        use crate::lower::graph_validation::tests::planned_names_for;

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
        let stubbed = partial
            .report
            .stubbed()
            .iter()
            .map(|stub| stub.name().to_owned())
            .collect::<HashSet<_>>();
        let legacy_functions = module_functions(&legacy.module_ir);
        let lowered_functions = module_functions(&partial.module_ir);
        let legacy_constants = module_constants(&legacy.module_ir);
        let lowered_constants = module_constants(&partial.module_ir);

        let mut compared = 0;
        for (legacy_name, entry) in &mapping.mapped {
            for planned in planned_names_for(program, entry) {
                if stubbed.contains(&planned) {
                    continue;
                }
                let legacy_body = legacy_functions
                    .get(legacy_name)
                    .unwrap_or_else(|| panic!("legacy `{legacy_name}` has no body ({label})"));
                let lowered_body = lowered_functions.get(&planned).unwrap_or_else(|| {
                    panic!("the lowered module has no body for `{planned}` ({label})")
                });
                let expected = normalize_function(legacy_body, &renames, &legacy_constants);
                let actual = normalize_function(lowered_body, &renames, &lowered_constants);
                assert_eq!(
                    actual, expected,
                    "normalized body differs for legacy `{legacy_name}` -> `{planned}` ({label})"
                );
                compared += 1;
            }
        }
        compared
    }
}
