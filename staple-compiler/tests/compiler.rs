// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CheckedMutation, CheckedType, CodeGenerator, EmissionUnit, LoweredModule, Lowerer,
    NameResolver, PlannedArtifactFamily, ProgramLoader, RecursiveConstruction, TypeChecker,
};
use staple_syntax::{Diagnostic, Item, Type, parse};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// `Result::expect_err`, but without ever `Debug`-formatting the `Ok` value.
///
/// A resolved or checked module embeds the whole `Program`, standard
/// library included, so if a test's precondition silently flips (the
/// source unexpectedly resolves/type-checks instead of failing) the
/// default `expect_err` panic message would try to format hundreds of
/// megabytes of nested AST into the panic payload instead of failing
/// fast — this looks exactly like a hang and can take many minutes.
trait ExpectErrDiagnostics {
    fn expect_err_diagnostics(self, message: &str) -> Vec<Diagnostic>;
}

impl<T> ExpectErrDiagnostics for Result<T, Vec<Diagnostic>> {
    fn expect_err_diagnostics(self, message: &str) -> Vec<Diagnostic> {
        match self {
            Err(diagnostics) => diagnostics,
            Ok(_) => panic!("{message}: expected an error, but resolution/type-checking succeeded"),
        }
    }
}

fn resolve(source: &str) -> staple_compiler::ResolvedModule {
    resolve_result(source).expect("source should resolve")
}

fn resolve_result(source: &str) -> Result<staple_compiler::ResolvedModule, Vec<Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let source = with_syntax_imports(source);
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(&source, root)
        .expect("source should load");
    NameResolver::new().resolve_program(program)
}

#[test]
fn applies_documentation_to_the_current_module() {
    let resolved = resolve("@doc(\"Module docs\")\npub mod\npub let answer = 42\n");
    assert_eq!(
        resolved.syntax().visibility,
        staple_syntax::Visibility::Public
    );
    assert_eq!(resolved.syntax().docs, ["Module docs"]);
}

#[test]
fn applies_user_modifiers_to_the_current_module() {
    let resolved = resolve(concat!(
        "@identity\n",
        "pub mod\n",
        "macro @identity: Item -> Item = item => item\n",
        "pub let answer = 42\n",
    ));
    assert_eq!(
        resolved.syntax().visibility,
        staple_syntax::Visibility::Public
    );
    assert!(
        resolved
            .syntax()
            .items
            .iter()
            .any(|item| matches!(item, Item::Binding(binding) if binding.name == "answer"))
    );
}

fn with_syntax_imports(source: &str) -> String {
    if source.contains("use std.syntax") {
        return source.to_owned();
    }
    let mut names = Vec::new();
    for (name, used) in [
        ("quote", source.contains("quote")),
        ("parse_quote", source.contains("parse_quote")),
        (
            "Expr",
            source.contains(": Expr") || source.contains("-> Expr"),
        ),
        (
            "Type",
            source.contains(": Type =>")
                || source.contains(": Type ->")
                || source.contains("-> Type"),
        ),
        (
            "Pattern",
            source.contains(": Pattern =>")
                || source.contains(": Pattern ->")
                || source.contains("-> Pattern"),
        ),
        (
            "Item",
            source.contains(": Item")
                || source.contains("-> Item")
                || source.contains("Sequence<Item>"),
        ),
        (
            "Syntax",
            source.contains(": Syntax")
                || source.contains("-> Syntax")
                || source.contains("Braced<Syntax>"),
        ),
        ("SyntaxNode", source.contains("SyntaxNode")),
        ("Ident", source.contains("Ident")),
        ("CallExpr", source.contains("CallExpr")),
        ("Sequence", source.contains("Sequence")),
        ("Separated", source.contains("Separated")),
        ("Comma", source.contains("Comma")),
        ("Equals", source.contains("Equals")),
        ("FatArrow", source.contains("FatArrow")),
        ("Parenthesized", source.contains("Parenthesized")),
        ("Bracketed", source.contains("Bracketed")),
        ("Braced", source.contains("Braced")),
        ("MacroCallMetadata", source.contains("MacroCallMetadata")),
        (
            "Visibility",
            source.contains(": Visibility") || source.contains("-> Visibility"),
        ),
    ] {
        if used && !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        source.to_owned()
    } else {
        format!("{source}\nuse std.syntax.({})\n", names.join(", "))
    }
}

fn type_check(source: &str) -> staple_compiler::TypedModule {
    TypeChecker::new()
        .check(resolve(source))
        .expect("source should type-check")
}

/// Lowers the program's own code. The standard library is treated as a
/// prebuilt object, as an incremental build links it, so only what the
/// program reaches is lowered. Emit the result with [`CompileRoot`].
fn lower(module: &staple_compiler::TypedModule) -> LoweredModule {
    Lowerer::new()
        .with_prebuilt_dependencies(true)
        .lower(module)
        .expect("checked module should lower")
}

/// Lowers the whole program, standard library included, for the
/// single-module build: tests that JIT-run the program, inspect standard
/// library code, or emit a complete object.
fn lower_whole(module: &staple_compiler::TypedModule) -> LoweredModule {
    Lowerer::new()
        .lower(module)
        .expect("checked module should lower")
}

/// Emits the IR of the program's own code: the root unit with the runtime.
/// Standard library functions are declared, not defined.
trait CompileRoot {
    fn compile_root(&self, module: &LoweredModule) -> Result<String, Vec<Diagnostic>>;
}

impl CompileRoot for CodeGenerator<'_> {
    fn compile_root(&self, module: &LoweredModule) -> Result<String, Vec<Diagnostic>> {
        self.compile_unit_for_target(module, &EmissionUnit::root(module).with_runtime(true), None)
    }
}

/// D2 assertions use the planned names of defined catalog artifacts.
fn assert_artifact_definition(llvm: &str, lowered: &LoweredModule, family: PlannedArtifactFamily) {
    let names = lowered.planned_artifact_names(family);
    assert!(!names.is_empty(), "expected a planned {family:?} artifact");
    assert!(
        names.iter().any(|name| llvm
            .lines()
            .any(|line| line.starts_with("define ") && line.contains(&format!("@{name}(")))),
        "expected a defined {family:?} artifact: {names:?}"
    );
}

/// Slices the LLVM definition of the function named `name` out of `llvm`,
/// from its `define` line up to the next definition.
fn function_definition<'a>(llvm: &'a str, name: &str) -> &'a str {
    llvm.split("\ndefine")
        .find(|definition| {
            definition
                .lines()
                .next()
                .is_some_and(|signature| signature.contains(&format!("@{name}(")))
        })
        .unwrap_or_else(|| panic!("`{name}` should be compiled"))
}

#[test]
fn block_arguments_lower_as_callbacks_and_preserve_callback_effects() {
    let module = type_check(concat!(
        "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = <T, effect E> callback => callback ()\n",
        "let mut count = 0\n",
        "let first = evaluate { count = count + 1; count }\n",
        "let second = evaluate { count }\n",
    ));

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("block arguments should lower as ordinary callbacks");
    assert!(llvm.contains("__staple_thunk_"));
}

/// A generic `def` whose type parameter never appears in its signature has no
/// binding storage, like every `<…>` binding, so its calls and values must
/// route through instances rather than a stored closure.
#[test]
fn phantom_type_parameter_generics_route_through_instances() {
    let module = type_check(concat!(
        "def phantom_top: <U where Copy U> () -> I32 = <U where Copy U> () => 4\n",
        "def captured: I32 -> I32 = value => { def phantom: <U where Copy U> I32 -> I32 = <U where Copy U> item => item + value; phantom.<Bool> 2 }\n",
        "def generic_outer: <T where Copy T> T -> I32 = <T where Copy T> value => { def phantom: <U where Copy U> () -> I32 = <U where Copy U> () => 4; phantom.<T> () }\n",
        "let direct = phantom_top.<I32> ()\n",
        "let callable: () -> I32 = phantom_top.<Bool>\n",
        "let through_value = callable ()\n",
        "let local = captured 5\n",
        "let nested = generic_outer 0\n",
    ));

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("phantom-parameter generic calls and values should emit");
}

#[test]
fn signals_and_scoped_reactions_type_check_and_lower() {
    let module = type_check(concat!(
        "let signal count = 0\n",
        "with Reactive = reactive_scope () {\n",
        "  reaction { let current = count; () }\n",
        "  count = 1\n",
        "}\n",
    ));

    let count = module
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == "count" => {
                module.symbol_for(binding.syntax.id)
            }
            _ => None,
        })
        .expect("signal binding");
    assert!(module.resolved().is_signal_symbol(count));
    assert_eq!(module.type_of_symbol(count), Some(&CheckedType::I32));

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("signals and reactions should lower");
    assert!(llvm.contains("__staple_signal_track"));
    assert!(llvm.contains("__staple_signal_notify"));
    assert!(llvm.contains("__staple_reaction_create"));
    assert!(llvm.contains("__staple_reactive_scope_dispose"));
}

#[test]
fn reactive_batches_type_check_and_lower_through_block_arguments() {
    let module = type_check(concat!(
        "let signal count = 0\n",
        "batch { count = 1; count = 2; () }\n",
    ));

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("reactive batches should lower");
    assert!(llvm.contains("__staple_batch_begin"));
    assert!(llvm.contains("__staple_batch_end"));
    assert!(llvm.contains("__staple_thunk_"));
    assert!(llvm.contains("__staple_executor_checkpoint"));
}

#[test]
fn infers_persistent_signal_derivations_and_respects_snapshot() {
    let module = type_check(concat!(
        "let signal count = 1\n",
        "let doubled = count + count\n",
        "let frozen = snapshot (count + count)\n",
    ));
    let mut bindings = module.syntax().items.iter().filter_map(|item| match item {
        Item::Binding(binding) => module
            .symbol_for(binding.syntax.id)
            .map(|symbol| (binding.name.as_str(), symbol)),
        _ => None,
    });
    let _count = bindings.next().expect("count");
    let (_, doubled) = bindings.next().expect("doubled");
    let (_, frozen) = bindings.next().expect("frozen");
    assert!(module.is_derived_symbol(doubled));
    assert!(!module.is_derived_symbol(frozen));

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("derived bindings should lower");
    assert!(llvm.contains("__staple_derived_create"));
    assert!(llvm.contains("__staple_derived_read"));
}

#[test]
fn reactive_provenance_flows_only_through_used_function_parameters() {
    let module = type_check(concat!(
        "let signal count = 1\n",
        "def double: I32 -> I32 = value => value + value\n",
        "def constant: I32 -> I32 = _ => 42\n",
        "let doubled = double count\n",
        "let fixed = constant count\n",
    ));
    let symbols = module
        .syntax()
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Binding(binding) => module
                .symbol_for(binding.syntax.id)
                .map(|symbol| (binding.name.as_str(), symbol)),
            _ => None,
        })
        .collect::<std::collections::HashMap<_, _>>();
    assert!(module.is_derived_symbol(symbols["doubled"]));
    assert!(!module.is_derived_symbol(symbols["fixed"]));
}

#[test]
fn derived_initializers_reject_independent_state_and_writes() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let signal count = 1\n",
            "let mut offset = 2\n",
            "let invalid = { offset = offset + 1; count + offset }\n",
        )))
        .expect_err_diagnostics("effectful derived initializers should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("derived binding must be pure") })
    );

    type_check(concat!(
        "let signal count = 1\n",
        "let mut offset = 2\n",
        "let valid = snapshot { offset = offset + 1; count + offset }\n",
    ));
}

#[test]
fn block_arguments_fill_fixed_product_argument_positions() {
    let module = type_check(concat!(
        "def add_later: (I32, () -> I32) -> I32 = (left, right) => left + right ()\n",
        "let block = add_later (40, { 2 })\n",
        "let direct = add_later (40, () => 2)\n",
    ));

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("fixed product slots should lower block arguments independently");
}

#[test]
fn block_arguments_are_not_a_general_value_coercion() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let callback: () -> I32 = 42\n"))
        .expect_err_diagnostics("non-call contexts must not thunk values");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `() -> I32`, found `I32`")
    }));
}

#[test]
fn trait_dispatch_selects_by_whether_the_argument_is_a_block() {
    let module = type_check(concat!(
        "trait Direct T { choose: T -> I32 }\n",
        "trait Lazy T { choose: (() -> T) -> I32 }\n",
        "impl Direct I32 { choose = value => value }\n",
        "impl Lazy I32 { choose = callback => callback () + 1 }\n",
        "let direct = choose 42\n",
        "let lazy = choose { 42 }\n",
    ));
    for item in &module.syntax().items {
        let Item::Binding(binding) = item else {
            continue;
        };
        assert_eq!(
            module.type_of_expression(binding.value.as_ref().unwrap().syntax().id),
            Some(&CheckedType::I32),
        );
    }
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("both trait candidates should lower");
}

#[test]
fn trait_methods_take_block_arguments_but_not_bare_values() {
    let module = type_check(concat!(
        "trait Lazy T { force: (() -> T) -> T }\n",
        "impl Lazy I32 { force = callback => callback () }\n",
        "let answer = force { 42 }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("trait dispatch should accept a block argument");

    TypeChecker::new()
        .check(resolve(concat!(
            "trait Lazy T { force: (() -> T) -> T }\n",
            "impl Lazy I32 { force = callback => callback () }\n",
            "let answer = force 42\n",
        )))
        .expect_err_diagnostics("a bare value is not a block argument");
}

fn rejected_messages(source: &str) -> Vec<String> {
    TypeChecker::new()
        .check(resolve(source))
        .expect_err_diagnostics("the program should be rejected")
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

#[test]
fn unbraced_values_are_not_block_arguments() {
    for source in [
        concat!(
            "def twice: (() -> ()) -> () = f => { f (); f () }\n",
            "def noop: () -> () = () => ()\n",
            "twice (noop ())\n",
        ),
        concat!(
            "def evaluate: (() -> I32) -> I32 = f => f ()\n",
            "let answer = evaluate 42\n",
        ),
    ] {
        let messages = rejected_messages(source);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("wrap the argument in braces")),
            "{messages:?}"
        );
    }
}

#[test]
fn block_arguments_always_thunk_and_grouping_opts_out() {
    // The block's value is a compatible function, but the braces still make
    // the block itself the callback, whose result must then be `()`.
    rejected_messages(concat!(
        "def twice: (() -> ()) -> () = f => { f (); f () }\n",
        "def make: () -> (() -> ()) = () => () => ()\n",
        "twice { make () }\n",
    ));
    // Grouping parentheses evaluate the block eagerly and pass its value.
    let module = type_check(concat!(
        "def twice: (() -> ()) -> () = f => { f (); f () }\n",
        "def make: () -> (() -> ()) = () => () => ()\n",
        "twice ({ make () })\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("an eagerly computed callback should lower");
    rejected_messages(concat!(
        "def evaluate: (() -> I32) -> I32 = f => f ()\n",
        "let answer = evaluate ({ 2 })\n",
    ));
}

#[test]
fn only_the_argument_itself_is_in_block_argument_position() {
    // The `match` fills the callback slot, so its block arms are ordinary
    // blocks rather than callbacks.
    rejected_messages(concat!(
        "def add_later: (I32, () -> I32) -> I32 = (left, right) => left + right ()\n",
        "let flag = True\n",
        "let answer = add_later (40, match flag { True() => { 1 }, False() => { 2 } })\n",
    ));
}

#[test]
fn block_arguments_fill_slots_but_not_pipe_receivers() {
    let module = type_check(concat!(
        "def direct: (I64; () -> I64) -> I64 = (value; callback) => value + callback ()\n",
        "let slotted: I64 = direct 2 { 3 }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slot block arguments should lower");

    // A pipe receiver is evaluated before the call, so it is never deferred.
    rejected_messages(concat!(
        "def force: (() -> I64) -> I64 = callback => callback ()\n",
        "let piped: I64 = { 3 } |> force\n",
    ));
}

#[test]
fn return_inside_a_block_argument_returns_from_the_callback() {
    let module = type_check(concat!(
        "def evaluate: (() -> I32) -> I32 = f => f ()\n",
        "def pick: Bool -> I32 = flag => {\n",
        "    let value = evaluate { if flag { return 1 }; 2 }\n",
        "    value + 10\n",
        "}\n",
        "let early = pick True\n",
        "let late = pick False\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("returns inside block arguments should lower");

    // The `return` is checked against the callback's `I32`, not the
    // enclosing function's `String`.
    let messages = rejected_messages(concat!(
        "def evaluate: (() -> I32) -> I32 = f => f ()\n",
        "def pick: Bool -> String = flag => {\n",
        "    evaluate { if flag { return \"early\" }; 2 }\n",
        "    \"late\"\n",
        "}\n",
    ));
    assert!(
        messages.iter().any(|message| message.contains("I32")),
        "{messages:?}"
    );
}

#[test]
fn break_and_continue_cannot_leave_a_block_argument() {
    for item in ["break", "continue"] {
        let messages = rejected_messages(&format!(
            "def run: (() -> ()) -> () = f => f ()\n\
             def spin: () -> () = () => {{\n\
                 loop {{ run {{ {item} }}; break }}\n\
             }}\n"
        ));
        assert!(
            messages
                .iter()
                .any(|message| message.contains(&format!("`{item}` cannot leave a block argument"))),
            "{messages:?}"
        );
    }
    type_check(concat!(
        "def run: (() -> ()) -> () = f => f ()\n",
        "run { loop { break } }\n",
    ));
}

#[test]
fn return_and_propagation_inside_a_coro_body_finish_the_coroutine() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "pub type Found<T> wrap pub T\n",
        "pub type Missing wrap pub String\n",
        "def lookup: Bool -> Found<I32> | Missing = available => match available {\n",
        "    True => Found(42),\n",
        "    False => Missing(\"not found\"),\n",
        "}\n",
        "def child: () -> Coroutine{}<I32> = () => coro { 5 }\n",
        "def pick: Bool -> Coroutine{}<I32> = flag => coro {\n",
        "    let first = await (child ())\n",
        "    if flag { return first }\n",
        "    first + 100\n",
        "}\n",
        "def propagate: Bool -> Coroutine{}<Found<I32> | Missing> = available => coro {\n",
        "    let ?Found(value) = lookup available\n",
        "    Found(value + value)\n",
        "}\n",
        "let early = block_on (pick True)\n",
        "let late = block_on (pick False)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("returns and propagation inside coro bodies should lower");

    // The `return` is checked against the coroutine's `I32`, not the
    // enclosing function's `Coroutine{}<I32>`.
    let messages = rejected_messages(concat!(
        "use std.coroutine.*\n",
        "def pick: Bool -> Coroutine{}<I32> = flag => coro {\n",
        "    if flag { return \"early\" }\n",
        "    2\n",
        "}\n",
    ));
    assert!(
        messages.iter().any(|message| message.contains("I32")),
        "{messages:?}"
    );
}

#[test]
fn break_and_continue_cannot_leave_a_coro_body() {
    // Name resolution treats a `coro` body as a function boundary, so a loop
    // outside it is not a target.
    for item in ["break", "continue"] {
        let diagnostics = resolve_result(&format!(
            "use std.coroutine.*\n\
             def spin: () -> () = () => {{\n\
                 loop {{ let held = coro {{ {item} }}; break }}\n\
             }}\n"
        ))
        .expect_err_diagnostics("a coro body cannot break an outer loop");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains(&format!("`{item}` is only allowed inside a loop"))),
            "{diagnostics:?}"
        );
    }
    type_check(concat!(
        "use std.coroutine.*\n",
        "def looped: () -> Coroutine{}<I32> = () => coro { loop { break 3 } }\n",
    ));
}

#[test]
fn specializes_generic_effect_parameters() {
    let module = type_check(concat!(
        "use std.io.(IO, println)\n",
        "def twice: <effect E> (() ->{E} ()) ->{E} () = <effect E> f => { f (); f () }\n",
        "def pure: () -> () = () => ()\n",
        "def output: () ->{IO} () = () => println \"hello\"\n",
        "let mut count = 0\n",
        "def update: () ->{state} () = () => { count = count + 1 }\n",
        "twice pure\ntwice output\ntwice update\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("effect-polymorphic calls should monomorphize");
}

/// A generic parameter used in a tuple argument beside a function over the
/// same parameter infers from both slots, with and without an effect
/// parameter, from concrete and generic bodies.
#[test]
fn infers_type_parameters_through_function_typed_tuple_slots() {
    let module = type_check(concat!(
        "use std.io.(IO, println)\n",
        "def show: I32 ->{IO} () = x => println \"${x}\"\n",
        "def apply: <T, effect E> (T, T ->{E} ()) ->{E} () = <T, effect E> (x, f) => f x\n",
        "def run: <A where Copy A> (A, A ->{IO} ()) ->{IO} () =\n",
        "    <A where Copy A> (a, g) => apply (a, g)\n",
        "def explicit: <A where Copy A> (A, A ->{IO} ()) ->{IO} () =\n",
        "    <A where Copy A> (a, g) => apply.<A, _> (a, g)\n",
        "apply (1, show)\nrun (2, show)\nexplicit (3, show)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("type-and-effect generic calls should compile");
}

/// Reachability: effect parameters that only ever receive concrete rows build
/// no bundles, and a polymorphic effect value that is never used builds no
/// dynamic code.
#[test]
fn effect_bundles_exist_only_for_run_time_rows() {
    let emit = |source: &str| {
        let module = type_check(source);
        let context = Context::create();
        CodeGenerator::new(&context)
            .compile_root(&lower(&module))
            .expect("effect programs should compile")
    };
    let direct = emit(concat!(
        "use std.io.(IO, println)\n",
        "def apply: <T, effect E> (T, T ->{E} ()) ->{E} () = <T, effect E> (x, f) => f x\n",
        "def show: I32 ->{IO} () = x => println \"${x}\"\n",
        "apply (1, show)\n",
    ));
    assert!(
        !direct.contains("effect.bundle"),
        "concrete rows need no bundle"
    );
    assert!(!direct.contains("_dynamic"), "no dynamic instance is built");

    let unused = emit(concat!(
        "def twice: <effect E> (() ->{E} ()) ->{E} () = <effect E> f => { f (); f () }\n",
        "def never: <T> T -> () = <T> x => {\n",
        "    let stored: <effect E> (() ->{E} ()) ->{E} () = twice\n",
        "    ()\n",
        "}\n",
    ));
    assert!(
        !unused.contains("_dynamic"),
        "an unused value builds no entry"
    );
    assert!(!unused.contains("effect.bundle"));

    let used = emit(concat!(
        "use std.io.(IO, println)\n",
        "def twice: <effect E> (() ->{E} ()) ->{E} () = <effect E> f => { f (); f () }\n",
        "let stored: <effect E> (() ->{E} ()) ->{E} () = twice\n",
        "def hello: () ->{IO} () = () => println \"hello\"\n",
        "stored hello\n",
    ));
    assert!(
        used.contains("_dynamic"),
        "a used value runs its dynamic entry"
    );
}

#[test]
fn checks_effect_parameterized_type_declarations() {
    type_check(concat!(
        "use std.io.(IO, println)\n",
        "type Callback{E} = () ->{E} ()\n",
        "type Handler{E}<T> = T ->{E} ()\n",
        "let pure: Callback = () => ()\n",
        "let io: Callback{IO} = () => println \"hello\"\n",
        "let stateful: Handler{state}<I32> = value => ()\n",
        "let empty: Handler<I32> = value => ()\n",
    ));
}

#[test]
fn coroutines_lower_to_resume_and_cleanup_functions() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "use std.io.(IO, println)\n",
        "def worker: () -> Coroutine{IO}<I32> = () => coro { println \"work\"; 7 }\n",
        "def driver: () -> Coroutine{IO}<I32> = () => coro { let v = await (worker ()); v + 1 }\n",
        "let answer = block_on (driver ())\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("coroutines should lower");
    // One state-machine `resume` (with a `state.0` entry block) and one
    // `cleanup` (with a `not.freed` block) per user coroutine body.
    assert_eq!(llvm.matches("\nstate.0:").count(), 2);
    assert_eq!(llvm.matches("\nnot.freed:").count(), 2);
    assert!(llvm.contains("@__staple_coro_"));
    // The frame is rooted at creation and unrooted in cleanup.
    assert!(llvm.contains("__staple_gc_register_root"));
    assert!(llvm.contains("__staple_gc_unregister_root"));
    // `block_on` and the nested `await` both drive a frame indirectly.
    assert!(llvm.contains("coro.result"));
}

/// A plain one-element product `(e)` is its element even when `e`'s type is a
/// product: `(println "x")` has type `()`. Lowering once read that type as
/// the parenthesized product's layout and rejected the program ("too many
/// positional elements in product").
#[test]
fn a_parenthesized_effectful_call_lowers() {
    let module = type_check(concat!(
        "use std.io.println\n",
        "(println \"short form\")\n",
        "let unit = (println \"bound\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a parenthesized effectful call should lower and compile");
}

#[test]
fn a_block_tail_coroutine_is_returned_instead_of_destroyed() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "def f: () -> Coroutine{}<I32> = () => { coro { 42 } }\n",
        "let held = f ()\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("a block-tail coroutine should lower");
    let body = function_definition(&llvm, "f");
    // Storing the `cleanup` pointer into the frame header is construction;
    // loading it back (`coro.cleanup.fn`) is what discarding the value emits.
    assert!(
        lowered
            .planned_artifact_names(PlannedArtifactFamily::CoroutineCodes)
            .iter()
            .filter(|name| name.ends_with("_cleanup"))
            .any(|name| body.contains(&format!("store ptr @{name}"))),
        "the block tail should construct a coroutine frame"
    );
    assert!(
        !body.contains("coro.cleanup.fn"),
        "a block tail must not drop its coroutine value"
    );
}

#[test]
fn dropping_an_unstarted_coroutine_emits_a_cleanup_call() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "use std.cinterop.(CString, c_string)\n",
        "def build: () -> Coroutine{}<()> =\n",
        "    () => {\n",
        "        let held = c_string \"captured\"\n",
        "        coro { let _ = held; () }\n",
        "    }\n",
        "let _ = build ()\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("an unstarted coroutine should lower");
    // The coroutine's cleanup exists and its drop path calls the closure
    // finalizer that frees the captured CString.
    assert!(llvm.contains("_cleanup(ptr"));
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::GcClosureFinalizer);
}

#[test]
fn awaiting_a_task_lowers_to_a_waiter_park_and_outcome_branch() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "use std.io.(IO, println)\n",
        "def leaf: () -> Coroutine{}<I32> = () => coro { 9 }\n",
        "def waiter: () -> Coroutine{Tasks, IO}<I32> = () => coro {\n",
        "    let t = spawn (leaf ())\n",
        "    let r = await t\n",
        "    match r {\n",
        "        Completed v => v,\n",
        "        Cancelled() => 0,\n",
        "    }\n",
        "}\n",
        "let sched = scheduler ()\n",
        "with Tasks = task_scope (sched) {\n",
        "    let _ = spawn (waiter ())\n",
        "    let _ = pump (sched, 8)\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("`await <Task>` should lower");
    // The awaiting body reads the external record and forks on whether it
    // completed or was cancelled.
    assert!(llvm.contains("external.record.state"));
    assert!(llvm.contains("await.ext.completed"));
    assert!(llvm.contains("await.ext.cancelled"));
    assert!(llvm.contains("await.ext.merge"));
    assert!(llvm.contains("__staple_task_await_register"));
    // The runtime driver gained the task-wait status and the record waiter/wake.
    assert!(llvm.contains("wake.waiter"));
    assert!(llvm.contains("wait_task"));
}

#[test]
fn completion_await_lowers_to_an_external_wait_with_a_fast_path() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "def observe: move Wait<I32> -> Coroutine{}<()> = move w => coro {\n",
        "    let outcome = await w\n",
        "    match outcome { Completed v => (), Cancelled() => () }\n",
        "}\n",
        "def make: Scheduler -> (wait: Wait<I32>, resolver: Resolver<I32>) = s => completion s\n",
        "let sched = scheduler ()\n",
        "with Tasks = task_scope (sched) {\n",
        "    let (w, r) = make sched\n",
        "    let _ = spawn (observe w)\n",
        "    Resolver.complete r 1\n",
        "    let _ = pump (sched, 4)\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("`await <Wait>` should lower");
    // Registration (with the already-resolved fast path), the shared outcome
    // branch, and the resolver/drop runtime shims.
    assert!(llvm.contains("__staple_completion_register"));
    assert!(llvm.contains("await.want.suspend"));
    assert!(llvm.contains("await.external.suspend"));
    assert!(llvm.contains("await.ext.completed"));
    assert!(llvm.contains("__staple_completion_complete"));
    assert!(llvm.contains("__staple_completion_wait_drop"));
    assert!(llvm.contains("__staple_completion_resolver_drop"));
}

#[test]
fn completion_with_cancel_arms_a_callback_and_the_unwind_abandons_it() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "def obs: move Wait<I32> -> Coroutine{}<()> = move w => coro {\n",
        "    let outcome = await w\n",
        "    match outcome { Completed v => (), Cancelled() => () }\n",
        "}\n",
        "def noop: () -> () = () => ()\n",
        "def prim: (Scheduler, () -> ()) -> (wait: Wait<I32>, resolver: Resolver<I32>) =\n",
        "    (s, cb) => completion_with_cancel (s, cb)\n",
        "let sched = scheduler ()\n",
        "with Tasks = task_scope (sched) {\n",
        "    let (w, r) = prim (sched, noop)\n",
        "    let h = spawn (obs w)\n",
        "    let _ = pump (sched, 4)\n",
        "    Task.cancel h\n",
        "    let _ = pump (sched, 4)\n",
        "    Resolver.cancel r\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("`completion_with_cancel` should lower");
    // The callback halves are stored and the armed bit is set at creation.
    assert!(llvm.contains("completion.cancel.fn"));
    assert!(llvm.contains("completion.cancel.env"));
    // The cancel unwind branches to an abandon path for a `Wait`-parked state.
    assert!(llvm.contains("cancel.abandon.wait"));
    assert!(llvm.contains("__staple_completion_abandon"));
    // The runtime distinguishes running the callback from releasing it.
    assert!(llvm.contains("__staple_completion_run_cancel"));
    assert!(llvm.contains("__staple_completion_disarm_cancel"));
}

#[test]
fn completion_tokens_expose_c_entry_points_and_a_staple_facade() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "def obs: move Wait<()> -> Coroutine{}<()> = move w => coro {\n",
        "    let outcome = await w\n",
        "    match outcome { Completed uu => (), Cancelled() => () }\n",
        "}\n",
        "def mk: Scheduler -> (wait: Wait<()>, token: CompletionToken) = s => completion_token s\n",
        "let sched = scheduler ()\n",
        "with Tasks = task_scope (sched) {\n",
        "    let (w, t) = mk sched\n",
        "    let _ = spawn (obs w)\n",
        "    let _ = pump (sched, 4)\n",
        "    CompletionToken.resolve t\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .expect("`completion_token` should lower");
    // The three host entry points are emitted with external linkage, and the
    // Staple `CompletionToken` facade calls into them / the shared cancel path.
    assert!(llvm.contains("define void @__staple_completion_token_resolve(ptr"));
    assert!(llvm.contains("define void @__staple_completion_token_cancel(ptr"));
    assert!(llvm.contains("define void @__staple_completion_token_release(ptr"));
    assert!(llvm.contains("call void @__staple_completion_token_resolve"));
}

#[test]
fn until_lowers_to_a_reaction_backed_completion_coroutine() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "let signal n = 0\n",
        "let sched = scheduler ()\n",
        "with Reactive = reactive_scope () {\n",
        "    with Tasks = task_scope (sched) {\n",
        "        let _ = spawn (coro {\n",
        "            let _ = await (until { n >= 5 })\n",
        "            ()\n",
        "        })\n",
        "        let _ = pump (sched, 4)\n",
        "    }\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("`until` should lower");
    // The `until` coroutine has its own resume/cleanup and an internal reaction
    // runner that resolves a completion.
    assert!(llvm.contains("define %CoroStatus @__staple_until_resume(ptr"));
    assert!(llvm.contains("define void @__staple_until_cleanup(ptr"));
    assert!(llvm.contains("@__staple_until_runner_"));
    assert!(llvm.contains("call ptr @__staple_reaction_create"));
    assert!(llvm.contains("until.predicate"));
    // A task cancelled while parked in `until` runs the child's cleanup.
    assert!(llvm.contains("cancel.cleanup.until"));
}

#[test]
fn until_rejects_a_predicate_that_is_not_pure_signal_reads() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "use std.io.(IO, println)\n",
            "let signal c = 0\n",
            "def bad: () -> Coroutine{Reactive, IO}<()> = () => coro {\n",
            "    let _ = await (until { println \"peek\"; c >= 3 })\n",
            "    ()\n",
            "}\n",
        )))
        .expect_err_diagnostics("an `until` predicate may only read signals");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("state.read")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

/// A `return` from inside `with Tasks` closes the scope on the
/// return path. The scope's normal exit is unreachable here, so any close in
/// `early` comes from the return; it must precede the function's `ret`.
#[test]
fn a_return_inside_a_task_scope_closes_it() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "def early: () -> I32 = () => {\n",
        "    let sched = scheduler ()\n",
        "    with Tasks = task_scope (sched) {\n",
        "        return 1\n",
        "    }\n",
        "    0\n",
        "}\n",
        "let value = early ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("an early return from a task scope should lower");
    let body = function_definition(&llvm, "early");
    assert_eq!(
        body.matches("call void @__staple_task_scope_close").count(),
        1,
        "the return path closes the scope exactly once:\n{body}"
    );
    let close = body
        .find("@__staple_task_scope_close")
        .expect("scope close");
    let ret = body.find("ret i32").expect("return");
    assert!(close < ret, "the close precedes the return:\n{body}");
}

/// `spawn`'s effect variable `E` is inferred from its coroutine argument
/// (`Coroutine{E}<T>`) inside an ordinary function, so the body's effect row is
/// the resolved one, not `{E}`. Effect-variable inference once considered only
/// function-typed arguments (such as `reaction`'s callback).
#[test]
fn spawn_type_checks_inside_an_ordinary_function() {
    type_check(concat!(
        "use std.coroutine.*\n",
        "use std.io.(IO, println)\n",
        "def worker: () -> Coroutine{Tasks, IO}<()> = () => coro {\n",
        "    let _ = await (yield_now ())\n",
        "    println \"task end\"\n",
        "}\n",
        "def early: Scheduler ->{IO} I32 = sched => {\n",
        "    with Tasks = task_scope (sched) {\n",
        "        let w: Coroutine{Tasks, IO}<()> = worker ()\n",
        "        let child = spawn w\n",
        "        let _ = pump (sched, 1)\n",
        "        return 1\n",
        "    }\n",
        "    0\n",
        "}\n",
        "let value = early (scheduler ())\n",
    ));
}

/// A coroutine's declared row is an upper bound on its body, like a function
/// declaration's: an over-declared `coro` takes the declared row (its frame's
/// resource bundle and every awaiter use it), so the coroutine type is the
/// declared one.
#[test]
fn a_declared_coroutine_row_bounds_its_body() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "use std.io.(IO, println)\n",
        "def quiet: () -> Coroutine{Tasks, IO}<I32> = () => coro { 5 }\n",
        "def outer: () -> Coroutine{Tasks, IO}<()> = () => coro {\n",
        "    let v = await (quiet ())\n",
        "    println \"awaited ${v:?}\"\n",
        "}\n",
        "let sched = scheduler ()\n",
        "with Tasks = task_scope (sched) {\n",
        "    let _ = spawn (outer ())\n",
        "    let _ = pump (sched, 8)\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("an over-declared coroutine should lower and compile");
}

/// A body that needs effects beyond its declared row is still rejected, both
/// directly and through a later, unannotated helper. On-demand inference may
/// diagnose the helper's effect before the global effect pass.
#[test]
fn a_coroutine_body_may_not_exceed_its_declared_row() {
    let direct = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "use std.io.(IO, println)\n",
            "def leaf: () -> Coroutine{}<I32> = () => coro { println \"x\"; 9 }\n",
        )))
        .expect_err_diagnostics("a direct effect beyond the declared row");
    assert!(
        direct.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected `Coroutine{}<I32>`, found `Coroutine{IO}<I32>`")),
        "unexpected diagnostics: {direct:?}",
    );
    let converged = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "use std.io.(IO, println)\n",
            "def leaf: () -> Coroutine{}<I32> = () => coro { helper (); 9 }\n",
            "def helper = () => println \"x\"\n",
        )))
        .expect_err_diagnostics("an inferred effect beyond the declared row");
    assert!(
        converged.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("coroutine body requires effects {IO}")
                || diagnostic
                    .message
                    .contains("expected `Coroutine{}<I32>`, found `Coroutine{IO}<I32>`")
        }),
        "unexpected diagnostics: {converged:?}",
    );
}

#[test]
fn spawned_coroutines_lower_a_cancellation_unwind_path() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "use std.io.(IO, println)\n",
        "def stepper: () -> Coroutine{Tasks, IO}<()> = () => coro {\n",
        "    println \"start\"\n",
        "    let _ = await (yield_now ())\n",
        "    println \"resumed\"\n",
        "}\n",
        "let sched = scheduler ()\n",
        "with Tasks = task_scope (sched) {\n",
        "    let t = spawn (stepper ())\n",
        "    let _ = pump (sched, 4)\n",
        "    Task.cancel t\n",
        "    let _ = pump (sched, 4)\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("cancellation should lower");
    // Every coroutine `resume` checks its task record for a cancel request and
    // branches to an unwind that reports CANCELLED.
    assert!(llvm.contains("cancel.check"));
    assert!(llvm.contains("cancel.unwind"));
    assert!(llvm.contains("resume.spent"));
    // `Task.cancel` and scope teardown call into the runtime.
    assert!(llvm.contains("__staple_task_cancel"));
    assert!(llvm.contains("__staple_task_scope_track"));
    assert!(llvm.contains("__staple_task_scope_close"));
}

#[test]
fn the_coroutine_driver_isolates_reactive_tracking_and_guards_pump() {
    let module = type_check(concat!(
        "use std.coroutine.*\n",
        "def leaf: () -> Coroutine{}<I32> = () => coro { 3 }\n",
        "def top: () -> Coroutine{}<I32> = () => coro { let r = await (leaf ()); r + 1 }\n",
        "let sched = scheduler ()\n",
        "with Tasks = task_scope (sched) {\n",
        "    let _ = spawn (top ())\n",
        "    let _ = pump (sched, 4)\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("coroutine reactive integration should lower");
    // The trampoline detaches dependency tracking around every `resume`.
    assert!(llvm.contains("__staple_tracking_suspend"));
    assert!(llvm.contains("__staple_tracking_restore"));
    // `pump` refuses to run inside a reaction or an open batch.
    assert!(llvm.contains("__staple_reactive_guard_active"));
}

#[test]
fn await_in_sub_expression_position_is_rejected() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "def get: () -> Coroutine{}<I32>\n",
            "def bad: () -> Coroutine{}<I32> =\n",
            "    () => coro { (await (get ())) + 1 }\n",
        )))
        .expect_err_diagnostics("`await` in operand position is not lowered in v1");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("must be a statement")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn coroutine_yield_type_and_empty_effect_row_default() {
    // `Coroutine<T>` is `Coroutine{}<T>`; an explicit `{IO}` row is retained.
    type_check(concat!(
        "use std.coroutine.*\n",
        "use std.io.(IO, println)\n",
        "let pure: Coroutine<I32> = coro { 1 }\n",
        "let also_pure: Coroutine{}<I32> = coro { 1 }\n",
        "let effectful: Coroutine{IO}<()> = coro { println \"hi\" }\n",
    ));
}

#[test]
fn macros_can_produce_coroutine_and_await_expressions() {
    type_check(concat!(
        "use std.syntax.(parse_quote, Expr)\n",
        "use std.coroutine.*\n",
        "macro spawn_pure: Expr -> Expr = value => parse_quote { coro { $value } }\n",
        "macro unwrap: Expr -> Expr = value => parse_quote { await $value }\n",
        "let job: Coroutine{}<I32> = spawn_pure 7\n",
        "let outer: Coroutine{}<I32> = coro { unwrap (spawn_pure 7) }\n",
    ));
}

#[test]
fn coroutine_construction_is_pure_and_await_incorporates_deferred_effects() {
    // `coro { ... }` defers its body's effects onto the `Coroutine{E}` type,
    // so a function that only *builds* a coroutine stays pure, while one that
    // `await`s it inside another `coro` body takes on the deferred row.
    type_check(concat!(
        "use std.coroutine.*\n",
        "use std.io.(IO, println)\n",
        "def build: () -> Coroutine{IO}<()> = () => coro { println \"hi\" }\n",
        "def run: () -> Coroutine{IO}<()> = () => coro { await (build ()) }\n",
    ));
}

#[test]
fn coroutine_body_effects_do_not_escape_to_the_enclosing_function() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "use std.io.(IO, println)\n",
            "def pure: () -> Coroutine{}<()> = () => coro { println \"hi\" }\n",
        )))
        .expect_err_diagnostics(
            "a coro body's IO effect must be reported against the coroutine type",
        );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("IO")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn await_outside_a_coro_body_is_rejected() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "def bad: Coroutine{}<I32> -> I32 = task => await task\n",
        )))
        .expect_err_diagnostics("`await` outside a coro body should be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("only allowed directly inside a `coro`")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn await_in_a_nested_function_inside_a_coro_is_rejected() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "def apply: <T> (() -> T) -> T = <T> f => f ()\n",
            "def bad: Coroutine{}<I32> -> Coroutine{}<I32> =\n",
            "    task => coro { apply (() => await task) }\n",
        )))
        .expect_err_diagnostics("`await` in a nested closure should be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("only allowed directly inside a `coro`")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn await_requires_a_coroutine_or_task_operand() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "def bad: () -> Coroutine{}<I32> = () => coro { await 42 }\n",
        )))
        .expect_err_diagnostics("`await` on a plain value should be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("requires a coroutine, task, or wait")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn coro_moves_owned_non_copy_captures() {
    // A `Task` handle is affine. Capturing it into a coroutine moves it, so a
    // second coroutine that captures the same handle is a use-after-move.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "def get_task: () -> Task<I32>\n",
            "def drop_it: <T> T -> () = <T> _ => ()\n",
            "def bad: () -> () =\n",
            "    () => {\n",
            "        let handle = get_task ()\n",
            "        drop_it (coro { await handle })\n",
            "        drop_it (coro { await handle })\n",
            "    }\n",
        )))
        .expect_err_diagnostics(
            "a coro that captures a task handle by move must forbid a later capture",
        );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("moved")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn double_await_of_a_task_handle_is_rejected() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.coroutine.*\n",
            "def get_task: () -> Task<I32>\n",
            "def bad: () -> Coroutine{}<I32> =\n",
            "    () => coro {\n",
            "        let handle = get_task ()\n",
            "        let _ = await handle\n",
            "        let _ = await handle\n",
            "        0\n",
            "    }\n",
        )))
        .expect_err_diagnostics("awaiting the same task handle twice is a use-after-move");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.to_lowercase().contains("move")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn coro_and_await_are_rejected_in_compile_time_evaluation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "use std.coroutine.*\nconst bad: I32 = await (coro { 1 })\n",
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("coroutines are not available at compile time");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("compile-time")),
        "unexpected diagnostics: {diagnostics:?}",
    );
}

#[test]
fn infers_open_effect_rows_and_checks_fixed_effects() {
    type_check(concat!(
        "use std.io.(IO, println)\n",
        "def run_and_log: <effect E> (() ->{E} ()) ->{E, IO} () = <effect E> f => { f (); println \"done\" }\n",
        "def pure: () -> () = () => ()\n",
        "let mut count = 0\n",
        "def update: () ->{state} () = () => { count = count + 1 }\n",
        "run_and_log pure\nrun_and_log update\n",
    ));
}

#[test]
fn infers_checks_and_lowers_typed_resources() {
    let module = type_check(concat!(
        "type Clock wrap (now: () -> I32)\n",
        "type Logger wrap (write: I32 -> ())\n",
        "def system_clock = Clock (now: () => 41)\n",
        "def logger = Logger (write: value => ())\n",
        "def read: () ->{Clock} I32 = () => (resource Clock).now ()\n",
        "def inferred = () => read ()\n",
        "def both: () ->{Logger, Clock} I32 = () => {\n",
        "  (resource Logger).write (inferred ())\n",
        "  inferred ()\n",
        "}\n",
        "with Clock = system_clock {\n",
        "  with Logger = logger { both () }\n",
        "}\n",
    ));

    let inferred = module
        .functions()
        .iter()
        .find(|function| function.name == "inferred")
        .expect("inferred function");
    let resources = &module
        .type_of_function(inferred.id)
        .expect("inferred function type")
        .effects;
    assert_eq!(resources.resources.len(), 1);
    assert_eq!(resources.resources[0].value_type.to_string(), "Clock");

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("typed resources should lower to hidden parameters");
    assert!(llvm.contains("define i32 @read"));
}

#[test]
fn captured_mutable_cells_contribute_state_effects_and_metadata() {
    use staple_compiler::CheckedStateEffect;

    let module = type_check(concat!(
        "use std.io.(IO, print)\n",
        "def make = () => {\n",
        "  let mut read_cell = 1\n",
        "  let mut write_cell = 2\n",
        "  let read = () => read_cell\n",
        "  let write = () => { write_cell = 3 }\n",
        "  let call_write = () => write ()\n",
        "  let both = () => { print \"x\"; write_cell = write_cell + 1 }\n",
        "  (read, write, call_write, both)\n",
        "}\n",
    ));

    let function = |suffix: &str| {
        module
            .functions()
            .iter()
            .find(|function| function.name.ends_with(suffix))
            .unwrap_or_else(|| panic!("function `{suffix}`"))
    };
    assert_eq!(
        module
            .type_of_function(function("read").id)
            .unwrap()
            .effects
            .state,
        Some(CheckedStateEffect::Read),
    );
    assert_eq!(
        module
            .type_of_function(function("write").id)
            .unwrap()
            .effects
            .state,
        Some(CheckedStateEffect::Write),
    );
    let both = module.type_of_function(function("both").id).unwrap();
    assert_eq!(both.effects.state, Some(CheckedStateEffect::ReadWrite));
    assert_eq!(both.effects.to_string(), "{state, IO}");

    let read_accesses = module
        .state_accesses_of_function(function("read").id)
        .unwrap();
    assert_eq!(read_accesses.reads.len(), 1);
    assert!(read_accesses.writes.is_empty());
    let write_accesses = module
        .state_accesses_of_function(function("write").id)
        .unwrap();
    assert!(write_accesses.reads.is_empty());
    assert_eq!(write_accesses.writes.len(), 1);
    assert_ne!(read_accesses.reads[0], write_accesses.writes[0]);
    assert_eq!(
        module
            .type_of_function(function("call_write").id)
            .unwrap()
            .effects
            .state,
        Some(CheckedStateEffect::Write),
    );
    assert_eq!(
        module
            .state_accesses_of_function(function("call_write").id)
            .unwrap(),
        write_accesses,
    );
}

#[test]
fn explicit_state_effects_are_checked_as_upper_bounds() {
    type_check(concat!(
        "def make = () => {\n",
        "  let mut value = 1\n",
        "  let read: () ->{state} I32 = () => value\n",
        "  let unused: () ->{state.write} () = () => ()\n",
        "  (read, unused)\n",
        "}\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def make = () => {\n",
            "  let mut value = 1\n",
            "  let bad: () ->{state.write} I32 = () => value\n",
            "  bad\n",
            "}\n",
        )))
        .expect_err_diagnostics("state.write does not cover a captured-state read");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("requires effects {state.read}") })
    );
}

#[test]
fn passing_a_captured_cell_to_a_mutating_parameter_is_a_state_write() {
    use staple_compiler::CheckedStateEffect;

    let module = type_check(concat!(
        "def replace: mut I32 -> () = mut value => { value = 2 }\n",
        "def make = () => {\n",
        "  let mut value = 1\n",
        "  let update = () => replace value\n",
        "  update\n",
        "}\n",
    ));
    let update = module
        .functions()
        .iter()
        .find(|function| function.name.ends_with("update"))
        .expect("update closure");
    assert_eq!(
        module.type_of_function(update.id).unwrap().effects.state,
        Some(CheckedStateEffect::Write),
    );
    let accesses = module.state_accesses_of_function(update.id).unwrap();
    assert!(accesses.reads.is_empty());
    assert_eq!(accesses.writes.len(), 1);
}

#[test]
fn mutable_module_bindings_contribute_state_effects_and_metadata() {
    use staple_compiler::CheckedStateEffect;

    let module = type_check(concat!(
        "let mut global = 1\n",
        "def read = () => global\n",
        "def write = () => { global = 2 }\n",
        "def both = () => { global = global + 1 }\n",
    ));
    let function = |name: &str| {
        module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("function `{name}`"))
    };

    assert_eq!(
        module
            .type_of_function(function("read").id)
            .unwrap()
            .effects
            .state,
        Some(CheckedStateEffect::Read),
    );
    assert_eq!(
        module
            .type_of_function(function("write").id)
            .unwrap()
            .effects
            .state,
        Some(CheckedStateEffect::Write),
    );
    assert_eq!(
        module
            .type_of_function(function("both").id)
            .unwrap()
            .effects
            .state,
        Some(CheckedStateEffect::ReadWrite),
    );

    let read = module
        .state_accesses_of_function(function("read").id)
        .unwrap();
    let write = module
        .state_accesses_of_function(function("write").id)
        .unwrap();
    let both = module
        .state_accesses_of_function(function("both").id)
        .unwrap();
    assert_eq!(read.reads, write.writes);
    assert_eq!(both.reads, read.reads);
    assert_eq!(both.writes, write.writes);
}

#[test]
fn rejects_invalid_resource_contracts_and_types() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid: () ->{(I32, I32)} () = () => ()\n"))
        .expect_err_diagnostics("structural resources must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("concrete, sized nominal type") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Clock wrap (now: () -> I32)\n",
            "def need = () => (resource Clock).now ()\n",
            "def invalid: () ->{} I32 = () => need ()\n",
        )))
        .expect_err_diagnostics("undeclared resources must be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("not contained in its declared effect set")
    }));
}

#[test]
fn mutable_and_non_copy_resources_follow_parameter_ownership() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "type Counter wrap (value: I32)\n",
        "type Handle wrap CString\n",
        "def increment: () ->{mut Counter} () = () => {\n",
        "  (resource Counter).value = (resource Counter).value + 1\n",
        "}\n",
        "def observe_handle: () ->{Handle} () = () => ()\n",
        "let mut counter = Counter (value: 0)\n",
        "with mut Counter = counter { increment () }\n",
    ));

    let increment = module
        .functions()
        .iter()
        .find(|function| function.name == "increment")
        .expect("increment function");
    let resources = &module
        .type_of_function(increment.id)
        .unwrap()
        .effects
        .resources;
    assert_eq!(resources.len(), 1);
    assert!(resources[0].mutable);

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("borrowed resources should lower through the hidden ABI");

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "type Handle wrap CString\n",
            "def take: move Handle -> () = move value => ()\n",
            "def invalid: () ->{Handle} () = () => take (resource Handle)\n",
        )))
        .expect_err_diagnostics("a borrowed resource must not be movable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("borrowed resource"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Counter wrap (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = 1\n",
            "}\n",
            "let counter = Counter (value: 0)\n",
            "with Counter = counter { increment () }\n",
        )))
        .expect_err_diagnostics("immutable providers cannot satisfy mutable resources");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use `with mut`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Counter wrap (value: I32)\n",
            "let counter = Counter (value: 0)\n",
            "with mut Counter = counter { () }\n",
        )))
        .expect_err_diagnostics("mutable providers need mutable named storage");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("provider must be declared `mut`")
    }));
}

#[test]
fn standard_io_is_a_compiler_provided_resource_and_propagates_to_main() {
    let module = type_check(concat!(
        "use std.io.(IO, print, println)\n",
        "def identity: <T> move T -> T = <T> move value => value\n",
        "def explicit: String ->{IO} () = value => print value\n",
        "def inferred = value: String => println value\n",
        "def main = () => { explicit \"one\"; inferred (identity \"two\") }\n",
    ));

    for name in ["explicit", "inferred", "main"] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name.ends_with(name))
            .expect("resource-bearing function");
        let resources = &module
            .type_of_function(function.id)
            .unwrap()
            .effects
            .resources;
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].value_type.to_string(), "IO");
    }
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generic calls should remain specialized during resource inference");
}

#[test]
fn allows_io_at_entry_module_top_level_but_rejects_non_builtin_opaque_resources() {
    // `IO` is implicitly available to the entry module's top-level
    // items, so a bare `println` there now compiles.
    type_check("use std.io.println\nprintln \"entry module output\"\n");

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "type Token opaque\ndef use_token: () ->{Token} () = () => ()\n",
        ))
        .expect_err_diagnostics("only std.io.IO may be an opaque resource");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("concrete, sized nominal type") })
    );
}

#[test]
fn resources_obey_alias_exactness_macro_trait_and_boundary_rules() {
    let module = type_check(concat!(
        "type Clock wrap I32\n",
        "type CurrentClock = Clock\n",
        "type Logger wrap I32\n",
        "type Box<T> wrap (value: T)\n",
        "trait Observe T { observe: T ->{Clock} Clock }\n",
        "impl Observe I32 { observe = value => resource CurrentClock }\n",
        "macro request = _: Ident<\"clock\"> => parse_quote { resource CurrentClock }\n",
        "def generated = () => request clock\n",
        "def declared: () ->{Logger, Clock, Clock} () = () => ()\n",
        "def through_trait = () => observe 1\n",
        "def boxed: () ->{Box<I32>} I32 = () => (resource Box<I32>).value\n",
        "with Clock = Clock 7 {\n",
        "  with Logger = Logger 8 { declared (); generated (); through_trait () }\n",
        "}\n",
        "with Box<I32> = Box (value: 9) { boxed () }\n",
    ));
    let declared = module
        .functions()
        .iter()
        .find(|function| function.name == "declared")
        .expect("declared function");
    assert_eq!(
        module
            .type_of_function(declared.id)
            .expect("declared function type")
            .effects
            .resources
            .len(),
        2
    );
    for name in ["generated", "through_trait"] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .expect("resource function");
        assert_eq!(
            module
                .type_of_function(function.id)
                .expect("resource function type")
                .effects
                .resources
                .len(),
            1
        );
    }
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("trait and specialized resource calls should lower");

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Clock wrap I32\n",
            "def need: () ->{Clock} Clock = () => resource Clock\n",
            "let incompatible: () -> Clock = need\n",
        )))
        .expect_err_diagnostics("resource-bearing and pure function types must compare exactly");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("expected `() -> Clock`")
            && diagnostic.message.contains("->{Clock}")
    }));

    for (source, expected) in [
        (
            "type Clock wrap I32\nresource Clock\n",
            "top-level initialization requires resources",
        ),
        (
            "type Clock wrap I32\nextern \"c\" { read: () ->{Clock} Clock }\n",
            "external functions cannot require Staple resources",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("resource boundary must be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected {expected:?}, got {diagnostics:?}"
        );
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro bad: Expr -> Expr = value: Expr => resource Expr\nlet generated = bad 1\n",
            root,
        )
        .expect("macro resource source should parse");
    let diagnostics = match NameResolver::new().resolve_program(program) {
        Err(diagnostics) => diagnostics,
        Ok(_) => panic!("resources must not be evaluated by macros"),
    };
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("resources are not available during compile-time macro evaluation")
    }));
}

#[test]
fn marks_compiler_owned_recursive_constructors() {
    let resolved = resolve("");
    let construction_for = |name: &str| {
        resolved
            .type_declarations()
            .iter()
            .find_map(|(id, declaration)| {
                (declaration.name == name).then(|| resolved.recursive_construction(*id))
            })
            .flatten()
    };

    assert_eq!(
        construction_for("Shared"),
        Some(RecursiveConstruction::ManagedReference)
    );
    assert_eq!(
        construction_for("CallExpr"),
        Some(RecursiveConstruction::Syntax)
    );
    assert_eq!(construction_for("String"), None);
}

#[test]
fn rejects_a_recursive_type_that_omits_the_self_reference_arguments() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "type Node<T> wrap (value: T, next: Option<Node>)\n",
        ))
        .expect_err_diagnostics("a bare generic self-reference is not a value type");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message
                == "type constructor `Node` expects 1 type argument, but 0 were supplied"
        }),
        "unexpected diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn rejects_an_alias_of_an_under_applied_type_constructor() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "type Entry<K, V> wrap (key: K, value: V)\ntype Partial = Entry<String>\n",
        ))
        .expect_err_diagnostics("an alias cannot name a partially applied type constructor");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message
                == "type constructor `Entry` expects 2 type arguments, but 1 was supplied"
        }),
        "unexpected diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn rejects_a_bare_generic_type_constructor_used_as_a_value_type() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let bad: Option = 1\n"))
        .expect_err_diagnostics("a partially applied type constructor is not a value type");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message
                == "type constructor `Option` expects 1 type argument, but 0 were supplied"
        }),
        "unexpected diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn allows_a_recursive_type_guarded_by_a_recursive_constructor() {
    type_check("use std.shared.(Shared)\ntype Node<T> wrap (value: T, next: Option<Shared<Node<I32>>>)\n");
    type_check("use std.shared.(Shared)\ntype List<T> wrap (head: T, tail: Option<Shared<List<T>>>)\n");
    type_check("use std.shared.(Shared)\ntype Chain wrap (head: I32, tail: Option<Shared<Chain>>)\n");
}

#[test]
fn constructs_a_recursive_value_through_its_own_indirection_arm() {
    // The recursive self-reference stand-in carries an `Opaque` placeholder
    // representation; passing `Shared node` back into the type's own constructor
    // makes generic inference for `Shared` reconcile that placeholder against the
    // fully-resolved `Node`, which must not be reported as a conflict.
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Node wrap () | (Shared<Node>)\n",
        "let node1 = Node ()\n",
        "let node2 = Node (Shared node1)\n",
        "let node3 = Node (Shared node2)\n",
    ));
}

#[test]
fn still_rejects_a_recursive_type_with_no_indirection() {
    TypeChecker::new()
        .check(resolve("type Loop wrap (head: I32, tail: Loop)\n"))
        .expect_err_diagnostics("a type that directly contains itself has no finite layout");
}

#[test]
fn recursive_constructor_guard_does_not_leak_into_a_nested_type() {
    // `Shared` is open while resolving `Outer`, but that must not license
    // `Inner`'s own by-value self-reference.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "type Inner wrap (here: I32, again: Inner)\n",
            "type Outer wrap (inner: Option<Shared<Inner>>)\n",
        )))
        .expect_err_diagnostics("a nested by-value cycle is still rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "cyclic type definition involving `Inner`"),
        "unexpected diagnostics: {diagnostics:#?}"
    );
}

fn copy_directory(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("test standard library directory should be created");
    for entry in std::fs::read_dir(source).expect("standard library directory should be readable") {
        let entry = entry.expect("standard library entry should be readable");
        let source = entry.path();
        let target = target.join(entry.file_name());
        if source.is_dir() {
            copy_directory(&source, &target);
        } else {
            std::fs::copy(source, target).expect("standard library file should be copied");
        }
    }
}

fn string_contract_diagnostics(declaration: &str) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after the Unix epoch")
        .as_nanos();
    let temporary = std::env::temp_dir().join(format!(
        "staple-compiler-string-contract-{}-{nonce}",
        std::process::id()
    ));
    copy_directory(&root.join("stdlib"), &temporary);
    let production_string = include_str!("../../stdlib/std/string.sta");
    let (production_prefix, production_body) = production_string
        .split_once("pub type String wrap SharedBuffer<U8>\n")
        .expect("production String module has its canonical declaration");
    std::fs::write(
        temporary.join("std/string.sta"),
        format!("{production_prefix}{declaration}\n{production_body}"),
    )
    .expect("test String declaration should be written");

    let messages = match ProgramLoader::new()
        .with_standard_library_root(&temporary)
        .load_source("", root)
    {
        Err(error) => vec![error],
        Ok(program) => match NameResolver::new().resolve_program(program) {
            Err(diagnostics) => diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.message)
                .collect(),
            Ok(module) => TypeChecker::new()
                .check(module)
                .err()
                .unwrap_or_default()
                .into_iter()
                .map(|diagnostic| diagnostic.message)
                .collect(),
        },
    };
    std::fs::remove_dir_all(temporary).expect("test standard library should be removed");
    messages
}

#[test]
fn supports_repeated_spread_and_slice_references() {
    let source = concat!(
        "use std.shared_buffer.SharedBuffer\n",
        "let explicit: (I32 ^ 3) = (1, 2, 3)\n",
        "let spread: (String, ...(I32 ^ 2)) = (\"x\", 4, 5)\n",
        "let fixed: (I32 ^ 3) = explicit\n",
        "let erased: SharedBuffer<I32> = SharedBuffer.from_array fixed\n",
        "let constructed: SharedBuffer<I32> = SharedBuffer.from_array (6, 7)\n",
        "let singleton: SharedBuffer<I32> = SharedBuffer.from_array 8\n",
        "let count: USize = SharedBuffer.length erased\n",
        "let fixed_count: USize = SharedBuffer.length (SharedBuffer.from_array fixed)\n",
        "let literal: I32 = erased.1\n",
        "let index: USize = 2\n",
        "let dynamic: I32 = erased[index]\n",
        "let fixed_dynamic: I32 = fixed[index]\n",
        "let direct: I32 = explicit[index]\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    let llvm = generator
        .compile_root(&lower(&module))
        .expect("product extensions should generate LLVM");
    assert!(llvm.contains("shared_buffer.length"));
    assert!(llvm.contains("index.out_of_bounds"));
    assert!(llvm.contains("llvm.trap"));
}

#[test]
fn from_array_accepts_generic_arrays_and_is_first_class() {
    let module = type_check(concat!(
        "use std.shared_buffer.SharedBuffer\n",
        "let fixed: (I32 ^ 3) = (1, 2, 3)\n",
        "let direct: SharedBuffer<I32> = SharedBuffer.from_array fixed\n",
        "let singleton: SharedBuffer<I32> = SharedBuffer.from_array 8\n",
        "let operation: (I32 ^ 3) -> SharedBuffer<I32> = SharedBuffer.from_array\n",
        "let applied: SharedBuffer<I32> = operation fixed\n",
        "def forward: <T, N where Natural N> move (T ^ N) -> SharedBuffer<T> = <T, N where Natural N> move value => SharedBuffer.from_array value\n",
        "let forwarded: SharedBuffer<I32> = forward fixed\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("`from_array` should accept a generic array and be first-class");
}

#[test]
fn checks_nullary_calls_inside_argument_positions() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "def take: <N where Natural N> Shared<(I32 ^ N)> -> I32 = <N where Natural N> value => 0\n",
        "let result: I32 = take (Shared ())\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a nullary call should check inside a call argument");
}

#[test]
fn supports_number_literal_types_as_generic_product_sizes() {
    let source = concat!(
        "type Three = 3\n",
        "type Vector<T, N where Natural N> = (T ^ N)\n",
        "let direct: (I32 ^ Three) = (1, 2, 3)\n",
        "let generic: Vector<I32, Three> = direct\n",
        "def keep: <T, N where Natural N> move (T ^ N) -> (T ^ N) = <T, N where Natural N> move values => values\n",
        "let inferred: (I32 ^ 3) = keep (1, 2, 3)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("dependent product sizes should specialize before code generation");
}

#[test]
fn number_literal_types_are_natural_usize_refinements() {
    type_check(concat!(
        "type Three = 3\n",
        "type Triple<T> = (T ^ Three)\n",
        "type NaturalIdentity<N where Natural N> = N\n",
        "type AlsoThree = NaturalIdentity<Three>\n",
        "let exact: 3 = 3\n",
        "let widened: USize = exact\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "type Bad<N where Natural N> = N\nlet invalid: Bad<USize> = 0\n",
        ))
        .expect_err_diagnostics("USize must not implement Natural");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("trait bound"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def invalid: <T, N> move (T ^ N) -> (T ^ N) = <T, N> move values => values\n",
        ))
        .expect_err_diagnostics("dependent sizes require a Natural trait bound");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("must have a `Natural` trait bound")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: 3 = 4\n"))
        .expect_err_diagnostics("literal annotations require the exact value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `3`, found `4`"))
    );
}

#[test]
fn natural_count_parameters_drive_repeated_product_values() {
    let source = concat!(
        "def repeat: <T, N where Copy T, Natural N> T -> N -> (T ^ N) = <T, N where Copy T, Natural N> value => n => (value ^ N)\n",
        "let repeated: (I32 ^ 3) = repeat 7 3\n",
        "type Count = 3\n",
        "let local: (I32 ^ 3) = (9 ^ Count)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("Natural-typed count parameters should specialize before code generation");

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def invalid: <T, N where Natural N> T -> N -> (T ^ N) = <T, N where Natural N> value => n => (value ^ N)\n",
        ))
        .expect_err_diagnostics("symbolic repetition requires Copy elements");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("symbolic length requires a `Copy` element type")
    }));
}

#[test]
fn natural_is_a_sealed_structural_trait() {
    for source in ["impl Natural USize {}\n", "impl !Natural 3 {}\n"] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("Natural implementations must be rejected");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("`Natural` is implemented structurally")
        }));
    }
}

#[test]
fn spreads_fixed_product_values_and_call_arguments() {
    let source = concat!(
        "def sum: (I32 ^ 4) -> I32 = (a, b, c, d) => a + b + c + d\n",
        "let pair = (left: 2, right: 3)\n",
        "let arguments = (2, 3)\n",
        "let expanded = (prefix: \"value\", ...pair, suffix: False)\n",
        "let selected: I32 = expanded.left + expanded.right\n",
        "let answer: I32 = sum (1, ...arguments, 4)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("product value spreads should generate LLVM");
    assert!(llvm.contains("product.spread.element"));
}

#[test]
fn repeated_product_values_expand_to_fixed_products() {
    let module = type_check(concat!(
        "let zeros: (I32 ^ 3) = (7 ^ 3)\n",
        "let sum: I32 = zeros.0 + zeros.1 + zeros.2\n",
        "let one: I32 = (5 ^ 1)\n",
        "let none = (0 ^ 0)\n",
    ));

    let items = &module.syntax().items;
    let Item::Binding(zeros) = &items[0] else {
        panic!("expected binding");
    };
    assert!(matches!(
        module.type_of_expression(zeros.value.as_ref().unwrap().syntax().id),
        Some(CheckedType::Product(product)) if product.elements.len() == 3
            && product.elements.iter().all(|element| element.value_type == CheckedType::I32),
    ));

    let Item::Binding(one) = &items[2] else {
        panic!("expected binding");
    };
    assert_eq!(
        module.type_of_expression(one.value.as_ref().unwrap().syntax().id),
        Some(&CheckedType::I32),
    );

    let Item::Binding(none) = &items[3] else {
        panic!("expected binding");
    };
    assert!(matches!(
        module.type_of_expression(none.value.as_ref().unwrap().syntax().id),
        Some(CheckedType::Product(product)) if product.elements.is_empty(),
    ));

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("repeated product values should generate LLVM");
}

#[test]
fn repeated_product_counts_must_be_types() {
    assert!(
        parse("let cells: (I32 ^ 3) = (1 ^ width + 1)\n").is_err(),
        "an array length is a type, not an expression"
    );
    assert!(
        parse("let cells: (I32 ^ 3) = (1 ^ width)\n").is_ok(),
        "a type name is a valid array length"
    );
}

#[test]
fn repeated_product_rejects_value_non_copy_and_oversized_counts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        "def make: I32 -> (I32 ^ 2) = size => (0 ^ size)\n",
        "let n: 3 = 3\nlet local: (I32 ^ 3) = (9 ^ n)\n",
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .expect("source should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("a value cannot be an array length");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .message
                    .contains("an array length must be a type satisfying `Natural`")
            }),
            "unexpected diagnostics: {diagnostics:?}",
        );
    }

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "let bad = (c_string \"x\" ^ 3)\n",
        )))
        .expect_err_diagnostics("a non-Copy element cannot be repeated");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("requires a `Copy` element type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "type Text = String\nlet bad: (I32 ^ 1) = (0 ^ Text)\n",
        ))
        .expect_err_diagnostics("a non-Natural type cannot be an array length");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("must satisfy `Natural`, found `String`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve("let huge = (0 ^ 70000)\n"))
        .expect_err_diagnostics("the arity limit is enforced");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("exceeds the limit") })
    );
}

#[test]
fn fills_named_type_field_defaults_at_calls_and_construction() {
    let source = concat!(
        "type Text from (String, x: I32 = 0, y: I32 = 0)\n",
        "type Point from (x: I32 = 1, y: I32 = 2)\n",
        "type Boxed wrap (value: I32 = 5, label: String = \"box\")\n",
        "def text: Text -> () = args => ()\n",
        "text (\"Hello\")\n",
        "text (\"Hello\", x: 5)\n",
        "text (\"Hello\", .y: 10)\n",
        "let point: Point = ()\n",
        "let shifted: Point = (.y: 3)\n",
        "let selected: I32 = point.x + shifted.y\n",
        "let boxed = Boxed ()\n",
        "let labelled = Boxed (.label: \"other\")\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("defaulted named-type fields should generate LLVM");
}

#[test]
fn rejects_same_name_functions_at_different_arities() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new().with_standard_library_root(root.join("stdlib")).load_source("def app: I32 -> I32 = value => value\ndef app: (I32; I32) -> I32 = (left; right) => left\n", root).expect("source loads");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("duplicate names must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("duplicate definition of `app`")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_a_default_on_a_curried_parameter() {
    let diagnostics = resolve_result("def bad: (value: I32 = 1) -> I32 = value => value\n")
        .expect_err_diagnostics("curried arrows cannot have defaults");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("only allowed in the representation of a `wrap` or `from` type")
    }));
}

#[test]
fn juxtaposed_functions_apply_extra_arguments_to_the_result() {
    let source = concat!(
        "def apply: I32 -> I32 -> I32 = first => second => first + second\n",
        "let through_unary: I32 = apply 1 2\n",
        "def extend: (I32; I32) -> I32 -> I32 = (first; second) => third => first + second + third\n",
        "let through_binary: I32 = extend 1 2 3\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("arguments after juxtaposed parameters should apply to its result");
}

#[test]
fn rejects_an_inference_placeholder_as_a_value() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid = _\n"))
        .expect_err_diagnostics("a standalone omission placeholder must fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("is not a value expression") })
    );
}

#[test]
fn rejects_same_scope_local_function_overloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new().with_standard_library_root(root.join("stdlib")).load_source("def run = () => {\n def local = value: I32 => value\n def local = (left: I32; right: I32) => left\n local 1\n}\n", root).expect("source loads");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("duplicate names must be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("duplicate definition of `local`")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_duplicate_functions_before_bare_name_use() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new().with_standard_library_root(root.join("stdlib")).load_source("def choose = value: I32 => value\ndef choose = (left: I32; right: I32) => left\nlet ambiguous = choose\n", root).expect("source loads");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("duplicate names must be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("duplicate definition of `choose`")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_trait_member_overloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "trait Render T { render: T -> T\n render: (T; T) -> T\n}\n",
            root,
        )
        .expect("source loads");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("duplicate names must be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("duplicate trait member `render`")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_companion_item_overloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new().with_standard_library_root(root.join("stdlib")).load_source("type Box wrap I32\ncompanion Box {\n pub def make: I32 -> Box = value => Box value\n pub def make: (I32; I32) -> Box = (left; right) => Box left\n}\n", root).expect("source loads");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("duplicate names must be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("duplicate definition of `make`")),
        "{diagnostics:?}"
    );
}

#[test]
fn field_defaults_apply_in_products_nested_in_a_representation() {
    let module = type_check(concat!(
        "type Config from (window: (width: I32 = 800, height: I32 = 600), title: String = \"app\")\n",
        "type Boxed wrap (inner: (a: I32 = 1, b: I32 = 2), c: I32 = 3)\n",
        "let default_config: Config = (window: ())\n",
        "let tall: Config = (window: (.height: 900), title: \"tall\")\n",
        "let boxed = Boxed (inner: (.b: 5))\n",
        "let width: I32 = default_config.window.width + tall.window.height + boxed.inner.a\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nested representation defaults should generate LLVM");
}

#[test]
fn field_defaults_may_name_singleton_values() {
    let module = type_check(concat!(
        "type Options wrap (verbose: Bool = False, depth: I32 = 1)\n",
        "let options = Options (.depth: 3)\n",
        "let quiet: Bool = options.verbose\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("singleton defaults should generate LLVM");
}

#[test]
fn named_type_field_defaults_combine_with_spread_fields() {
    let source = concat!(
        "type Base = (x: I32, y: I32)\n",
        "type Point3 from (...Base, z: I32 = 3)\n",
        "let point: Point3 = (1, 2)\n",
        "let answer: I32 = point.x + point.y + point.z\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("defaults after spread fields should generate LLVM");
}

#[test]
fn rejects_invalid_product_field_defaults_and_missing_required_fields() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Bad from (x: I32 = \"wrong\", y: I32 = 0)\n",
            "let bad: Bad = ()\n",
        )))
        .expect_err_diagnostics("a mismatched default should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `I32`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("type Bad from (x: I32 = 1)\nlet bad: Bad = 1\n"))
        .expect_err_diagnostics("a singleton default should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("singleton product field"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Args from (a: I32, b: I32 = 1, c: I32)\n",
            "def bad: Args -> () = args => ()\n",
            "bad (a: 1)\n",
        )))
        .expect_err_diagnostics("a required trailing field should not be defaulted");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing product field `c`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def make: I32 -> () = captured => {\n",
            "    type Bad from (x: I32 = captured, y: I32 = 0)\n",
            "    let bad: Bad = ()\n",
            "    ()\n",
            "}\n",
        )))
        .expect_err_diagnostics("a default should not capture a local value");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot capture a local runtime value")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let mut state: I32 = 1\n",
            "type Bad from (x: I32 = state, y: I32 = 0)\n",
            "let bad: Bad = ()\n",
        )))
        .expect_err_diagnostics("a stateful default should not be pure");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("must be pure"))
    );
}

#[test]
fn rejects_field_defaults_outside_named_type_representations() {
    let diagnostics = resolve_result(concat!(
        "let point: (x: I32 = 0, y: I32 = 0) = ()\n",
        "def text: (String, x: I32 = 0) -> () = (value, x) => ()\n",
        "type Alias = (x: I32 = 0, y: I32 = 0)\n",
        "type Callback wrap (call: (x: I32 = 0, y: I32) -> I32, c: I32 = 2)\n",
        "type Items wrap (items: List<(x: I32 = 0, y: I32)>)\n",
    ))
    .expect_err_diagnostics("defaults belong to `wrap`/`from` representations only");
    let rejected = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message
                == "field default values are only allowed in the representation of a `wrap` or `from` type declaration"
        })
        .count();
    // Both point fields, the parameter field, both alias fields, and the
    // products inside a representation's function type and type argument;
    // `c` belongs to the representation itself.
    assert_eq!(rejected, 7, "{diagnostics:?}");
}

#[test]
fn spreads_named_product_values_by_name_and_overrides_fields() {
    let source = concat!(
        "let dimensions = (height: 600, width: 800)\n",
        "let config: (width: I32, height: I32, title: String) = (\n",
        "    ...=dimensions,\n",
        "    title: \"Staple\",\n",
        ")\n",
        "let overridden: (width: I32, height: I32) = (\n",
        "    ...=dimensions,\n",
        "    width: 900,\n",
        ")\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("named product value spreads should generate LLVM");
    assert!(llvm.contains("product.spread.element"));
}

#[test]
fn rejects_invalid_named_product_spreads() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let dimensions = (height: 600, width: 800)\n",
            "let config = (...=dimensions, title: \"Staple\")\n",
        )))
        .expect_err_diagnostics("a named spread requires a known expected product type");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("requires a fully-named expected product type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let dimensions = (height: 600, width: 800)\n",
            "let config: (width: I32, height: I32, title: String) = (...=dimensions)\n",
        )))
        .expect_err_diagnostics("a required field left unfilled should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing field `title`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let dimensions = (height: 600, width: 800)\n",
            "let config: (width: I32, height: I32) = (...=dimensions, extra: 1)\n",
        )))
        .expect_err_diagnostics("a field absent from the expected type should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown field"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let unnamed = (1, 2)\n",
            "let config: (width: I32, height: I32) = (...=unnamed)\n",
        )))
        .expect_err_diagnostics("an operand with unnamed elements cannot be named-spread");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("must have every element named"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let dimensions = (height: 600, width: 800)\n",
            "let config: (width: I32, height: I32) = (...dimensions, ...=dimensions)\n",
        )))
        .expect_err_diagnostics("a positional spread cannot combine with a named spread");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot combine a positional spread with a named spread")
    }));
}

#[test]
fn constructs_products_with_contextual_named_initializers() {
    let source = concat!(
        "let value: (I32, I32, a: I32, b: I32) = (1, 2, .b: 4, .a: 3)\n",
        "let result: I32 = value.0 + value.1 + value.a + value.b\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("designated product should generate LLVM");
}

#[test]
fn rejects_invalid_contextual_named_initializers_and_labels() {
    let cases = [
        (
            "let value = (.a: 1, .b: 2)\n",
            "require a known expected product shape",
        ),
        (
            "let value: (a: I32, b: I32) = (.a: 1, .missing: 2)\n",
            "unknown designated product field `missing`",
        ),
        (
            "let value: (a: I32, b: I32) = (.a: 1)\n",
            "missing product field `b`",
        ),
        (
            "let value: (a: I32, b: I32) = (wrong: 1, 2)\n",
            "expected product field label `a`, found `wrong`",
        ),
        (
            "let value: (I32, b: I32) = (wrong: 1, 2)\n",
            "does not match an unnamed expected position",
        ),
        (
            "let value: (a: I32, a: I32) = (1, 2)\n",
            "duplicate product field name `a`",
        ),
        (
            "let value = (a: 1, a: 2)\n",
            "duplicate product field name `a`",
        ),
        (
            "let pair = (a: 1, b: 2)\nlet value: (x: I32, b: I32) = (...pair)\n",
            "expected product field label `x`, found `a`",
        ),
        (
            "let pair = (a: 1, b: 2)\nlet value = (...pair, a: 3)\n",
            "duplicate product field name `a`",
        ),
        (
            "type Pair = (a: I32, b: I32)\ndef value: (...Pair, a: I32)\n",
            "duplicate product field name `a`",
        ),
    ];
    for (source, expected) in cases {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("invalid product should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected diagnostic containing `{expected}`, got {diagnostics:?}"
        );
    }
}

#[test]
fn designated_initializers_override_earlier_values() {
    // A `.name:` designator overrides the positional prefix and any earlier
    // designator for the same field, matching `...=` merge semantics.
    let source = concat!(
        "let value: (a: I32, b: I32) = (1, 2, .a: 10, .a: 30)\n",
        "let result: I32 = value.a + value.b\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("designated override should generate LLVM");
}

#[test]
fn keeps_named_spread_override_behavior() {
    let source = concat!(
        "let original = (a: 1, b: 2)\n",
        "let value: (a: I32, b: I32) = (...=original, a: 3)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("named spread overrides should remain valid");
}

#[test]
fn type_checks_and_lowers_mutable_places_and_shared_replace() {
    let source = concat!(
        "use std.shared.(Shared)\n",
        "let mut value = 1\n",
        "value = 2\n",
        "let mut pair = (x: 3, y: 4)\n",
        "pair.x = value\n",
        "let mut fixed: Shared<(I32 ^ 2)> = Shared (5, 6)\n",
        "fixed.0 = pair.x\n",
        "let index: USize = 1\n",
        "fixed[index] = 7\n",
        "let mut scalar = Shared 8\n",
        "let old = Shared.replace scalar 9\n",
        "def local = () => { let mut inside = 10; inside = old; inside }\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("mutable places should generate LLVM");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("place.field"));
    assert!(llvm.contains("shared.replace.old"));
}

#[test]
fn type_checks_the_two_binding_mutability_forms() {
    // `let`: neither reassignable nor mutable.
    let diagnostics = TypeChecker::new()
        .check(resolve("let a = (x: 1, y: 2)\na = (x: 3, y: 4)\n"))
        .expect_err_diagnostics("`let` cannot be reassigned");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
    let diagnostics = TypeChecker::new()
        .check(resolve("let a = (x: 1, y: 2)\na.x = 3\n"))
        .expect_err_diagnostics("`let` cannot be written through");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );

    // `let mut`: both reassignable and mutable.
    type_check("let mut a = (x: 1, y: 2)\na = (x: 3, y: 4)\na.x = 5\n");

    // Function parameters are writable when their inferred or declared
    // effect covers the corresponding position. Match binders still use an
    // explicit `mut` pattern.
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Box wrap I32\n",
        "def parameter = (value: I32) => { let mut value = value; value = value + 1; value }\n",
        "def field_write: mut Shared<(x: I32, y: I32)> -> () = mut value => { value.x = 1 }\n",
        "def matched = (value: Box) => match value { Box (mut inner) => { inner = 2; inner } }\n",
    ));

    type_check("def by_value: mut (x: I32, y: I32) -> () = mut value => { value.x = 1 }\n");

    // A `pub` global follows the same rules from another module as it does locally.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Empty\n",
            "pub let a = (x: 1, y: 2)\n",
            "a.x = 3\n",
        )))
        .expect_err_diagnostics("`pub let` cannot be written through even in its own module");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn writes_through_a_shared_require_mut_on_a_named_root() {
    // A named `Shared`-typed binding needs `mut` to be written through.
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "let mut cell: Shared<(I32 ^ 2)> = Shared (1, 2)\n",
        "cell.0 = 3\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.shared.(Shared)\nlet cell: Shared<(I32 ^ 2)> = Shared (1, 2)\ncell.0 = 3\n",
        ))
        .expect_err_diagnostics("writing through a `Shared` requires `mut` on the binding");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );

    // A rootless `Shared` temporary remains writable regardless.
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "def make_shared = () => Shared (3, 4)\n",
        "(make_shared ()).0 = 5\n",
    ));
}

#[test]
fn captures_a_mut_binding_in_a_shared_cell_for_field_writes() {
    let module = type_check(concat!(
        "def make = () => {\n",
        "  let mut point = (x: 1, y: 2)\n",
        "  let update = () => { point.x = point.x + 1; point.x }\n",
        "  update ()\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("captured mut binding should generate LLVM");
    assert!(llvm.contains("binding.cell"));
}

fn function_mutations(module: &staple_compiler::TypedModule, name: &str) -> Vec<CheckedMutation> {
    let function = module
        .functions()
        .iter()
        .find(|function| function.name == name)
        .unwrap_or_else(|| panic!("function `{name}`"));
    module
        .type_of_function(function.id)
        .unwrap_or_else(|| panic!("type of `{name}`"))
        .mutations
        .clone()
}

#[test]
fn a_parameter_marker_declares_a_positional_mutation() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "def f = (mut p: Shared<(I32, I32)>) => { p.0 = 1 }\n",
        "def g = () => { let mut pair: Shared<(I32, I32)> = Shared (1, 2); f pair }\n",
    ));
    assert_eq!(
        function_mutations(&module, "f"),
        vec![CheckedMutation::Element(0)]
    );
}

#[test]
fn rejects_a_declared_empty_effect_set_when_the_body_mutates_a_parameter() {
    let diagnostics = TypeChecker::new()
        .check(resolve("use std.shared.(Shared)\ndef f: Shared<(I32, I32)> -> () = p => { p.0 = 1 }\n"))
        .expect_err_diagnostics("an empty declared effect set forbids mutation");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn rejects_a_declared_mutation_target_the_body_does_not_write() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "def f: (mut a: Shared<(I32, I32)>, b: Shared<(I32, I32)>) -> () = (a, b) => { b.0 = 1 }\n",
        )))
        .expect_err_diagnostics("writing `b` exceeds the declared `mut a`");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn resolves_named_and_positional_mutation_targets_to_the_same_type() {
    let by_index =
        type_check("use std.shared.(Shared)\ndef f: (mut a: Shared<(I32, I32)>, b: I32) -> () = (mut a, b) => { a.0 = 1 }\n");
    let by_name =
        type_check("use std.shared.(Shared)\ndef f: (mut a: Shared<(I32, I32)>, b: I32) -> () = (mut a, b) => { a.0 = 1 }\n");
    assert_eq!(
        function_mutations(&by_index, "f"),
        function_mutations(&by_name, "f"),
    );
}

#[test]
fn a_whole_parameter_declaration_covers_writing_a_single_element() {
    type_check("use std.shared.(Shared)\ndef f: mut Shared<(I32, I32)> -> () = mut p => { p.0 = 1 }\n");
}

#[test]
fn call_site_mutation_requires_an_explicit_caller_marker() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "def f: mut Shared<(I32, I32)> -> () = mut p => { p.0 = 1 }\n",
            "def g = (q: Shared<(I32, I32)>) => { f q }\n",
        )))
        .expect_err_diagnostics("mutation permissions must not propagate into callers");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );

    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "def f: mut Shared<(I32, I32)> -> () = mut p => { p.0 = 1 }\n",
        "def g = (mut q: Shared<(I32, I32)>) => { f q }\n",
    ));
    assert_eq!(
        function_mutations(&module, "g"),
        vec![CheckedMutation::Element(0)]
    );
}

#[test]
fn rejects_passing_a_non_mut_binding_to_a_mutating_parameter() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "def f: mut Shared<(I32, I32)> -> () = p => { p.0 = 1 }\n",
            "def g = () => { let pair: Shared<(I32, I32)> = Shared (1, 2); f pair }\n",
        )))
        .expect_err_diagnostics("the caller's binding must be declared `mut`");
    assert!(diagnostics.iter().any(|d| {
        d.message
            .contains("cannot write through `pair`; its binding is not declared `mut`")
    }));
}

#[test]
fn a_closure_may_mutate_an_explicitly_marked_captured_parameter() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "def outer = (mut p: Shared<(I32, I32)>) => {\n",
        "  let update = () => { p.0 = 1 }\n",
        "  update ()\n",
        "}\n",
    ));
    assert_eq!(
        function_mutations(&module, "outer"),
        vec![CheckedMutation::Element(0)]
    );
}

#[test]
fn rejects_an_impl_member_that_mutates_beyond_its_trait_declaration() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "trait Reset T { reset: T -> () }\n",
            "impl Reset Shared<(I32, I32)> { reset = p => { p.0 = 0 } }\n",
        )))
        .expect_err_diagnostics("the impl mutates a parameter its trait does not declare");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn an_explicit_mut_effect_passes_through_a_shared_crossing_local_alias() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "type MyInt wrap Shared<I32>\n",
        "def mutate_my_int: (mut MyInt, I32) -> () = (mut my_int, value) => {\n",
        "  let MyInt mut inner = my_int\n",
        "  Shared.replace inner value\n",
        "  ()\n",
        "}\n",
        "def foo = (mut my_int: MyInt) => { mutate_my_int (my_int, 42) }\n",
    ));
    assert_eq!(
        function_mutations(&module, "foo"),
        vec![CheckedMutation::Element(0)]
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "type MyInt wrap Shared<I32>\n",
            "def mutate_my_int: (mut MyInt, I32) -> () = (mut my_int, value) => {\n",
            "  let MyInt mut inner = my_int\n",
            "  Shared.replace inner value\n",
            "  ()\n",
            "}\n",
            "def bad = () => {\n",
            "  let my_int: MyInt = MyInt (Shared 1)\n",
            "  mutate_my_int (my_int, 42)\n",
            "}\n",
        )))
        .expect_err_diagnostics("the caller's `my_int` must be declared `mut` too");
    assert!(diagnostics.iter().any(|d| {
        d.message
            .contains("cannot write through `my_int`; its binding is not declared `mut`")
    }));
}

/// The number of `%`-named values in a function's LLVM `define` line: one
/// per parameter (the closure environment, any hidden resources, and the
/// function's own flattened parameters), regardless of their types.
fn llvm_parameter_count(llvm: &str, function: &str) -> usize {
    let needle = format!("@{function}(");
    let start = llvm
        .find(&needle)
        .unwrap_or_else(|| panic!("no `define` for `{function}` in:\n{llvm}"));
    let signature = &llvm[start..];
    let end = signature
        .find(')')
        .unwrap_or_else(|| panic!("unterminated parameter list for `{function}`"));
    signature[..end].matches('%').count()
}

#[test]
fn a_mut_effect_adds_no_hidden_parameter_to_the_abi() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Clock wrap I32\n",
        "def f: mut Shared<(I32, I32)> -> () = mut p => { p.0 = 1 }\n",
        "def g: mut Shared<(I32, I32)> ->{Clock} () = mut p => { p.0 = 1 }\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("mut effects should not affect the ABI");
    let base = llvm_parameter_count(&llvm, "f");
    let with_resource = llvm_parameter_count(&llvm, "g");
    // Adding the `Clock` resource on top of the same `mut` effect adds
    // exactly its own one hidden parameter, not a second one for `mut`
    // (which contributes nothing to the ABI either way).
    assert_eq!(with_resource, base + 1);
}

#[test]
fn a_mutated_shared_parameter_lowers_and_the_caller_observes_the_write() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "def f: mut Shared<(I32, I32)> -> () = mut p => { p.0 = p.0 + 1 }\n",
        "def use_it = () => { let mut pair: Shared<(I32, I32)> = Shared (1, 2); f pair; pair.0 }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a mutated Shared parameter should lower");
}

#[test]
fn a_mut_parameter_can_replace_the_callers_value() {
    let module = type_check(concat!(
        "def update_data: mut I32 -> () = mut data => { data = 42 }\n",
        "let mut data: I32 = 1\n",
        "update_data data\n",
    ));
    assert_eq!(
        function_mutations(&module, "update_data"),
        vec![CheckedMutation::Whole]
    );
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a replaced scalar parameter should lower by address");
    assert!(llvm.contains("define") && llvm.contains("@update_data(ptr"));
    assert!(llvm.contains("store i32 42"));
}

#[test]
fn a_parameter_marker_adds_mutation_without_a_type_annotation() {
    let module = type_check(concat!(
        "def update_data = (mut data: I32) => { data = 42 }\n",
        "let mut data: I32 = 1\n",
        "update_data data\n",
    ));
    assert_eq!(
        function_mutations(&module, "update_data"),
        vec![CheckedMutation::Element(0)]
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid = (data: I32) => { data = 42 }\n"))
        .expect_err_diagnostics("an unmarked parameter must not infer mutation");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn parameter_markers_must_match_explicit_function_and_trait_effects() {
    type_check(concat!(
        "def replace: mut I32 -> () = mut value: I32 => { value = 1 }\n",
        "trait Replace T { replace: mut T -> () }\n",
        "impl Replace I32 { replace = mut value => { value = 2 } }\n",
    ));

    for source in [
        "def mismatch: (I32, mut I32) -> () = (mut first, second) => ()\n",
        concat!(
            "trait Replace T { replace: (mut T, T) -> () }\n",
            "impl Replace I32 { replace = (first, mut second) => () }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics(
                "parameter markers and declared mutation permissions must match",
            );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("parameter `mut` markers declare")
        }));
    }
}

#[test]
fn parameter_move_markers_must_match_explicit_function_and_trait_effects() {
    for source in [
        "def mismatch: move I32 -> () = value => ()\n",
        concat!(
            "trait Consume T { consume: move T -> () }\n",
            "impl Consume I32 { consume = value => () }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("parameter `move` markers and declared ownership must match");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("parameter `move` markers declare")
        }));
    }
}

#[test]
fn resource_inference_preserves_parameter_declared_mutation() {
    let module = type_check(concat!(
        "type Clock wrap I32\n",
        "def use_both = (mut value: Clock) => { value = resource Clock }\n",
    ));
    let function = module
        .functions()
        .iter()
        .find(|function| function.name == "use_both")
        .expect("use_both function");
    let function_type = module.type_of_function(function.id).unwrap();
    assert_eq!(function_type.mutations, vec![CheckedMutation::Element(0)]);
    assert_eq!(function_type.effects.resources.len(), 1);
}

#[test]
fn whole_and_positional_product_parameters_can_be_replaced() {
    let module = type_check(concat!(
        "def replace_pair: mut (I32, I32) -> () = mut pair => { pair = (3, 4) }\n",
        "def replace_first: (mut I32, I32) -> () = (mut first, _) => { first = 9 }\n",
        "let mut pair = (1, 2)\n",
        "replace_pair pair\n",
        "let mut first = 0\n",
        "replace_first (first, 8)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("whole and positional product replacements should lower");
}

#[test]
fn a_first_class_mutating_function_preserves_writeback() {
    let module = type_check(concat!(
        "def replace: mut I32 -> () = mut value => { value = 7 }\n",
        "let operation: mut I32 -> () = replace\n",
        "let mut value = 1\n",
        "operation value\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("indirect mutating calls should use the address-passing ABI");
}

#[test]
fn a_mut_call_materializes_and_discards_a_temporary_argument() {
    let module = type_check(concat!(
        "def update_data: mut I32 -> () = mut data => { data = 42 }\n",
        "update_data (1 + 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a temporary mut argument should get call-scoped storage");
    assert!(llvm.contains("mutation.temporary"));
}

#[test]
fn a_move_only_mut_temporary_drops_replaced_and_final_values() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "def replace: mut CString -> () = mut value => { value = c_string \"replacement\" }\n",
        "replace (c_string \"initial\")\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("move-only mutation temporaries should lower with ownership cleanup");
    assert!(llvm.contains("mutation.temporary.final"));
    assert!(llvm.contains("assignment.old"));
}

#[test]
fn rejects_invalid_assignment_targets_and_uninitialized_mutable_lets() {
    let error = parse("let mut value: I32\n").expect_err("`let` bindings require an initializer");
    assert!(error.message.contains("require an initializer"));

    let diagnostics = TypeChecker::new()
        .check(resolve("let value = 1\nvalue = 2\n"))
        .expect_err_diagnostics("immutable names cannot be reassigned");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("not declared `mut`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def product = () => (x: 1, y: 2)\nproduct ().x = 3\n",
        ))
        .expect_err_diagnostics("by-value temporaries are not writable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("writable place"))
    );
}

#[test]
fn lowers_move_only_mutation_reinitialization_and_captured_cells() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "use std.cinterop.*\n",
        "def local = (move first: CString, move second: CString) => {\n",
        "  let mut value = first\n",
        "  value = second\n",
        "  let moved = value\n",
        "  value = c_string \"replacement\"\n",
        "  drop moved\n",
        "  drop value\n",
        "}\n",
        "def captured = (move initial: CString) => {\n",
        "  let mut value = initial\n",
        "  () => { let old = value; value = c_string \"next\"; drop old }\n",
        "}\n",
        "def managed = (move initial: CString, move next: CString) => {\n",
        "  let mut reference = Shared initial\n",
        "  let old = Shared.replace reference next\n",
        "  drop old\n",
        "}\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("move-only mutable cells should generate LLVM");
    assert!(llvm.contains("cell.drop.is_live"));
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::GcCellFinalizer);
    assert!(llvm.contains("shared.replace.old"));
}

#[test]
fn supports_mutable_parameter_match_and_copy_shared_pattern_binders() {
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Box wrap I32\n",
        "type Empty\n",
        "def parameter = (value: I32) => { let mut value = value; value = value + 1; value }\n",
        "def matched = (value: Box | Empty) => match value { Box (mut inner) => { inner = 3; inner }, Empty() => 0 }\n",
        "def borrowed = (value: Shared<I32>) => { let Shared (mut inner) = value; inner = 4; inner }\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "type Resource wrap I32\n",
            "impl Drop Resource { cleanup = Resource value => () }\n",
            "def invalid = (value: Shared<Resource>) => { let Shared (mut inner) = value; inner }\n",
        )))
        .expect_err_diagnostics("move-only Shared borrows cannot become mutable locals");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("borrowed through `Shared` cannot be bound as `mut`")
    }));
}

#[test]
fn rejects_invalid_product_spreads_and_indices() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid: (...I32)\n"))
        .expect_err_diagnostics("a scalar cannot be spread");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot spread non-product type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid = (...1, 2)\n"))
        .expect_err_diagnostics("a scalar value cannot be spread");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot spread non-product value")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.CString\n",
            "def invalid = (pair: (CString, I32), position: USize) => pair[position]\n",
        )))
        .expect_err_diagnostics("a product containing a move-only element cannot derive Index");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("no `Index` implementation") })
    );
}

#[test]
fn derives_trait_delegated_product_indexing() {
    let source = concat!(
        "use std.shared.(Shared)\n",
        "use std.shared_buffer.SharedBuffer\n",
        "def select: <A, B where Copy A, Copy B> ((A, B), USize) -> A | B = <A, B where Copy A, Copy B> (pair, position) => pair[position]\n",
        "let pair = (1, \"two\")\n",
        "let position: USize = 1\n",
        "let generic_selected: I32 | String = select (pair, position)\n",
        "let collapsed = select ((1, 2), position)\n",
        "let selected: I32 | String = pair[position]\n",
        "let operation: ((I32, String), USize) -> I32 | String = Index.index\n",
        "let selected_again: I32 | String = operation (pair, position)\n",
        "let mut updated: (I32 ^ 3) = (1, 2, 3)\n",
        "updated[position] = 9\n",
        "let mutate_by_value: (mut (I32 ^ 3), USize, I32) -> () = MutateIndex.mutate_index\n",
        "mutate_by_value (updated, position, 10)\n",
        "let mut fixed: Shared<(I32 ^ 3)> = Shared updated\n",
        "let mutate_operation: (mut Shared<(I32 ^ 3)>, USize, I32) -> () = MutateIndex.mutate_index\n",
        "mutate_operation (fixed, position, 6)\n",
        "fixed[position] = 7\n",
        "let mut erased: SharedBuffer<I32> = SharedBuffer.from_array updated\n",
        "erased[position] = 8\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("derived indexing traits should generate LLVM");
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::StructuralIndex);
    assert_artifact_definition(
        &llvm,
        &lowered,
        PlannedArtifactFamily::StructuralMutateIndex,
    );
    assert!(llvm.contains("index.case"));
}

#[test]
fn delegates_brackets_to_explicit_indexing_implementations() {
    let source = concat!(
        "type Target wrap I32\n",
        "impl Index Target String { type Output = Target; index = (target, position) => target }\n",
        "impl MutateIndex Target String { type Value = Target; mutate_index = (mut target, position, move value) => () }\n",
        "let selected: Target = (Target 4)[\"key\"]\n",
        "(Target 4)[\"key\"] = Target 5\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("custom indexing traits should generate LLVM");
}

#[test]
fn allows_by_value_indexed_assignment_for_mut_bindings() {
    let source =
        "let mut values: (I32 ^ 2) = (1, 2)\nlet position: USize = 0\nvalues[position] = 3\n";
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("by-value indexed assignment through a `mut` binding should generate LLVM");
}

#[test]
fn rejects_by_value_indexed_assignment_without_mut_binding() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let values: (I32 ^ 2) = (1, 2)\nlet position: USize = 0\nvalues[position] = 3\n",
        ))
        .expect_err_diagnostics("by-value product assignment requires a `mut` binding");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("not declared `mut`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.shared.(Shared)\nlet values: Shared<(I32 ^ 2)> = Shared (1, 2)\nvalues[2] = 3\n",
        ))
        .expect_err_diagnostics("known out-of-bounds MutateIndex must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("out of bounds"))
    );
}

#[test]
fn derives_mutate_index_for_move_only_homogeneous_products() {
    let source = concat!(
        "use std.shared.(Shared)\n",
        "use std.cinterop.CString\n",
        "def mutate_by_value = (mut values: (CString ^ 2), position: USize, move replacement: CString) => { ",
        "values[position] = replacement; () }\n",
        "def mutate = (mut values: Shared<(CString ^ 2)>, position: USize, move replacement: CString) => { ",
        "values[position] = replacement; () }\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("move-only indexed replacement should generate LLVM");
    assert!(llvm.contains("index.old"));
}

#[test]
fn rejects_overlapping_structural_indexing_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "impl Index (I32 ^ 2) USize { type Output = I32;\n",
            "  index = (values, position) => values.0\n",
            "}\n",
        )))
        .expect_err_diagnostics("structural product Index cannot be overridden");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("derived structurally for this type")
    }));
}

#[test]
fn delegates_indexing_through_shared_values_to_the_payload() {
    let source = concat!(
        "use std.shared.(Shared)\n",
        "use std.list.List\n",
        "use std.shared_buffer.SharedBuffer\n",
        "def mixed_at: (Shared<(I32, Bool)>, USize) -> I32 | Bool = (pair, position) => pair[position]\n",
        "def list_at: (Shared<List<I32>>, USize) -> I32 = (list, position) => list[position]\n",
        "let fixed: (I32 ^ 3) = (1, 2, 3)\n",
        "let values: SharedBuffer<I32> = SharedBuffer.from_array fixed\n",
        "def slice_at: (Shared<SharedBuffer<I32>>, USize) -> I32 = (slice, position) => slice[position]\n",
        "type Keyed wrap (key: String, value: I32)\n",
        "impl Index Keyed String { type Output = I32; index = (entry, key) => entry.value }\n",
        "def keyed_at: (Shared<Keyed>, String) -> I32 = (entry, key) => entry[key]\n",
        "def nested_at: (Shared<Shared<(I32 ^ 3)>>, USize) -> I32 = (values, position) => values[position]\n",
        "def fixed_at: (Shared<(I32 ^ 3)>, USize) -> I32 = (values, position) => values[position]\n",
        "def generic_at: <T where Index T USize> (Shared<T>, USize) -> Index.Output T USize = <T where Index T USize> (values, position) => values[position]\n",
        "let list = List.of (1, 2, 3)\n",
        "let value: I32 = generic_at (Shared (Clone.clone list), 0)\n",
        "let operation: (Shared<List<I32>>, USize) -> I32 = Index.index\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("indexing through a Shared should generate LLVM");
    assert!(llvm.contains("index.deref"));
}

#[test]
fn delegates_indexed_assignment_through_shared_values_to_the_payload() {
    let source = concat!(
        "use std.shared.(Shared)\n",
        "use std.list.List\n",
        "def set_list = (mut list: Shared<List<I32>>, position: USize, value: I32) => { list[position] = value }\n",
        "def set_nested = (mut values: Shared<Shared<(I32 ^ 2)>>, position: USize, value: I32) => { values[position] = value }\n",
        "def set_fixed = (mut values: Shared<(I32 ^ 2)>, position: USize, value: I32) => { values[position] = value }\n",
        "type Counter wrap I32\n",
        "impl MutateIndex Counter String { type Value = I32; mutate_index = (mut counter, key, move value) => () }\n",
        "def set_keyed = (mut counter: Shared<Counter>, key: String, value: I32) => { counter[key] = value }\n",
        "let list = List.of (1, 2, 3)\n",
        "let operation: (mut Shared<List<I32>>, USize, I32) -> () = MutateIndex.mutate_index\n",
        "let mut reference = Shared (Clone.clone list)\n",
        "operation (reference, 0, 9)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("indexed assignment through a Shared should generate LLVM");
    assert!(llvm.contains("mutate_index.deref"));
}

#[test]
fn rejects_out_of_bounds_indices_known_through_a_shared() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.shared.(Shared)\ndef invalid = (values: Shared<(I32 ^ 2)>) => values[2]\n",
        ))
        .expect_err_diagnostics("a known out-of-bounds Shared index must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("out of bounds"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.shared.(Shared)\ndef invalid = (values: Shared<Shared<(I32 ^ 2)>>) => values[2]\n",
        ))
        .expect_err_diagnostics("a known out-of-bounds nested Shared index must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("out of bounds"))
    );
}

#[test]
fn indexes_slices_through_the_standard_library_implementation() {
    let source = concat!(
        "use std.shared_buffer.SharedBuffer\n",
        "let fixed: (I32 ^ 3) = (1, 2, 3)\n",
        "let mut values: SharedBuffer<I32> = SharedBuffer.from_array fixed\n",
        "let value: I32 = values[0]\n",
        "values[1] = 9\n",
        "let operation: (SharedBuffer<I32>, USize) -> I32 = Index.index\n",
        "let same: I32 = operation (values, 2)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slice indexing should use the standard-library implementation");
    assert!(!llvm.contains("structural_Index"));
    assert!(!llvm.contains("structural_MutateIndex"));
}

#[test]
fn iterates_slices_through_the_standard_library_implementations() {
    let source = concat!(
        "use std.shared_buffer.(SharedBuffer, SharedBufferIter)\n",
        "let fixed: (I32 ^ 3) = (10, 20, 30)\n",
        "let values: SharedBuffer<I32> = SharedBuffer.from_array fixed\n",
        "let iterator: SharedBufferIter<I32> = IntoIterator.into_iterator values\n",
        "let step: IterStep<SharedBufferIter<I32>, I32> = Iterator.next iterator\n",
        "def sum: SharedBuffer<I32> -> I32 = slice => {\n",
        "  let mut total = 0\n",
        "  for value in slice { total = total + value }\n",
        "  total\n",
        "}\n",
        "let total: I32 = sum values\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slice iteration should use the standard-library implementation");
}

#[test]
fn compares_slices_and_strings_through_the_standard_library_eq_implementations() {
    let module = type_check(concat!(
        "use std.shared_buffer.SharedBuffer\n",
        "use std.cmp.Eq\n",
        "let first: (I32 ^ 3) = (1, 2, 3)\n",
        "let same: (I32 ^ 3) = (1, 2, 3)\n",
        "let different: (I32 ^ 3) = (1, 2, 4)\n",
        "let shorter: (I32 ^ 2) = (1, 2)\n",
        "let left: SharedBuffer<I32> = SharedBuffer.from_array first\n",
        "let right: SharedBuffer<I32> = SharedBuffer.from_array same\n",
        "let other: SharedBuffer<I32> = SharedBuffer.from_array different\n",
        "let short: SharedBuffer<I32> = SharedBuffer.from_array shorter\n",
        "let same_values: Bool = left == right\n",
        "let different_values: Bool = left != other\n",
        "let different_lengths: Bool = left != short\n",
        "def generic_equal: <T where Eq T> (SharedBuffer<T>, SharedBuffer<T>) -> Bool = <T where Eq T> (x, y) => x == y\n",
        "let generic: Bool = generic_equal (left, right)\n",
        "let first_text: String = \"staple\"\n",
        "let second_text: String = \"staple\"\n",
        "let other_text: String = \"staples\"\n",
        "let same_text: Bool = first_text == second_text\n",
        "let different_text: Bool = first_text != other_text\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slice and string equality should use the standard-library implementations");
}

#[test]
fn rejects_slice_equality_when_the_element_is_not_eq() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared_buffer.SharedBuffer\n",
            "use std.cinterop.CString\n",
            "def invalid: (SharedBuffer<CString>, SharedBuffer<CString>) -> Bool = (left, right) => left == right\n",
        )))
        .expect_err_diagnostics("a slice of a non-Eq element must not be comparable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation"))
    );
}

#[test]
fn rejects_slice_iteration_for_non_copy_elements() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared_buffer.SharedBuffer\n",
            "use std.cinterop.CString\n",
            "def invalid: SharedBuffer<CString> -> () = values => {\n",
            "  for value in values { () }\n",
            "}\n",
        )))
        .expect_err_diagnostics("a slice of a move-only element cannot be iterated");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation"))
    );
}

#[test]
fn rejects_explicit_indexing_implementations_for_shared_targets() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "type Target wrap I32\n",
            "impl Index Target String { type Output = Target; index = (target, position) => target }\n",
            "impl Index Shared<Target> String { type Output = Target; index = (target, position) => target }\n",
        )))
        .expect_err_diagnostics("Shared indexing is derived from the payload's implementation");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("derived structurally for this type")
    }));
}

#[test]
fn auto_derefs_fields_through_nested_shared_values_and_slices() {
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "use std.shared_buffer.SharedBuffer\n",
        "def slice_field: Shared<SharedBuffer<I32>> -> I32 = values => values.0\n",
        "def nested_field = (point: Shared<Shared<(x: I32, y: I32)>>) => point.x\n",
        "def nested_write = (mut point: Shared<Shared<(x: I32, y: I32)>>) => { point.x = 1; () }\n",
    ));
}

#[test]
fn derives_iterator_item_as_a_sum_of_product_elements() {
    type_check(concat!(
        "let iterator: ((I32 ^ 3), USize) = IntoIterator.into_iterator (1, 2, 3)\n",
        "let homogeneous_step: IterStep<((I32 ^ 3), USize), I32> = Iterator.next iterator\n",
        "let mixed: (I32, String, I32) = (1, \"two\", 3)\n",
        "let mixed_iterator: ((I32, String, I32), USize) = IntoIterator.into_iterator mixed\n",
        "let mixed_step: IterStep<((I32, String, I32), USize), I32 | String> = Iterator.next mixed_iterator\n",
    ));
}

#[test]
fn derives_structural_iteration_for_products() {
    let source = concat!(
        "def sum_pair = pair: (I32, I32) => {\n",
        "  let mut total = 0\n",
        "  for value in pair { total = total + value }\n",
        "  total\n",
        "}\n",
        "let homogeneous_total: I32 = sum_pair (1, 2)\n",
        "def classify = value: I32 | String => match value {\n",
        "  number: I32 => number,\n",
        "  text: String => 0,\n",
        "}\n",
        "def sum_mixed = triple: (I32, String, I32) => {\n",
        "  let mut total = 0\n",
        "  for value in triple { total = total + classify(value) }\n",
        "  total\n",
        "}\n",
        "let mixed_total: I32 = sum_mixed (1, \"two\", 3)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("derived iteration traits should generate LLVM");
    assert_artifact_definition(
        &llvm,
        &lowered,
        PlannedArtifactFamily::StructuralIntoIterator,
    );
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::StructuralIterator);
    assert!(llvm.contains("next.case"));
    assert!(llvm.contains("loop.body"));
}

#[test]
fn rejects_structural_iteration_for_non_copy_products() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.CString\n",
            "def invalid = pair: (CString, I32) => {\n",
            "  let mut count = 0\n",
            "  for value in pair { count = count + 1 }\n",
            "  count\n",
            "}\n",
        )))
        .expect_err_diagnostics(
            "a product containing a move-only element cannot derive IntoIterator",
        );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation"))
    );
}

#[test]
fn rejects_overlapping_structural_iterator_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "impl IntoIterator (I32 ^ 2) { type Iter = ((I32 ^ 2), USize);\n",
            "  into_iterator = move value => (value, USize :: 0)\n",
            "}\n",
        )))
        .expect_err_diagnostics("structural product IntoIterator cannot be overridden");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("derived structurally for this type")
    }));
}

#[test]
fn rejects_slice_shared_destructuring() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.shared.(Shared)\nuse std.shared_buffer.SharedBuffer\nlet fixed: (I32 ^ 2) = (1, 2)\nlet values: SharedBuffer<I32> = SharedBuffer.from_array fixed\nlet Shared payload = values\n",
        ))
        .expect_err_diagnostics("a slice cannot be destructured as a Shared");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("cannot be destructured") })
    );
}

#[test]
fn handles_array_edges_and_limits() {
    type_check("let value: (...(I32 ^ 0), ...(I32 ^ 1), I32) = (1, 2)\n");

    let diagnostics = TypeChecker::new()
        .check(resolve("def too_large: (I32 ^ 65536)\n"))
        .expect_err_diagnostics("oversized arrays must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("limit of 65535") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let pair: (I32 ^ 2) = (1, 2)\nlet invalid = pair[2]\n",
        ))
        .expect_err_diagnostics("known out-of-bounds indices must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("out of bounds") })
    );
}

#[test]
fn aliases_complete_slice_references_and_reject_ffi() {
    type_check(concat!(
        "use std.shared_buffer.SharedBuffer\n",
        "type Ints = SharedBuffer<I32>\n",
        "let fixed: (I32 ^ 2) = (1, 2)\n",
        "let values: Ints = SharedBuffer.from_array fixed\n",
        "let count: USize = SharedBuffer.length values\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.shared_buffer.SharedBuffer\nextern \"c\" { invalid: SharedBuffer<I32> -> I32 }\n",
        ))
        .expect_err_diagnostics("slices must not cross the FFI");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("external binding types cannot contain `SharedBuffer` values")
    }));
}

#[test]
fn enforces_implicit_sized_and_supports_question_sized_parameters() {
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "use std.shared_buffer.SharedBuffer\n",
        "def preserve_slice: <T> SharedBuffer<T> -> SharedBuffer<T> = <T> value => value\n",
        "def explicitly_sized: <T where ?Sized T, Sized T> Shared<T> -> Shared<T> = <T where ?Sized T, Sized T> value => value\n",
        "let fixed: (I32 ^ 2) = (1, 2)\n",
        "let erased: SharedBuffer<I32> = SharedBuffer.from_array fixed\n",
        "let same: SharedBuffer<I32> = preserve_slice erased\n",
        "let same_fixed: Shared<(I32 ^ 2)> = explicitly_sized (Shared fixed)\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def invalid: <T where ?Sized T> T -> () = <T where ?Sized T> value => ()\n",
        ))
        .expect_err_diagnostics("a relaxed parameter cannot be passed by value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("must be sized") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("impl Sized I32 {}\n"))
        .expect_err_diagnostics("Sized is structural");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`Sized` is implemented structurally")
    }));
}

#[test]
fn rejects_question_sized_clauses_naming_unknown_or_repeated_parameters() {
    for (source, message) in [
        (
            "type Bad<T where ?Sized U> = T\n",
            "`?Sized` must name an already introduced compile-time parameter; `U` was not found",
        ),
        (
            "type Bad<T where ?Sized T, ?Sized T> = T\n",
            "duplicate `?Sized` clause for compile-time parameter `T`",
        ),
        (
            "def bad: <T where ?Sized U> Shared<T> -> Shared<T> = <T> value => value\n",
            "`U` was not found",
        ),
    ] {
        let diagnostics = resolve_result(source).expect_err(source);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(message)),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn member_modifiers_document_and_track_trait_members() {
    let module = resolve(concat!(
        "trait Describe T {\n",
        "    /// First line.\n",
        "    @doc(\"Second line.\")\n",
        "    @track_caller\n",
        "    describe: T -> I32\n",
        "}\n",
    ));
    let describe = module
        .traits()
        .values()
        .find(|resolved| resolved.declaration.name == "Describe")
        .expect("trait");
    let member = &describe.declaration.members[0];
    assert_eq!(member.docs, [" First line.", "Second line."]);
    assert!(member.track_caller);

    for (source, message) in [
        (
            "trait Bad T {\n    @track_caller\n    type Out\n    f: T -> T\n}\n",
            "`@track_caller` may only modify a trait member function",
        ),
        (
            "trait Bad T {\n    @track_caller(1)\n    f: T -> T\n}\n",
            "`@track_caller` takes no argument",
        ),
        (
            "trait Bad T {\n    @inline\n    f: T -> T\n}\n",
            "only `@doc` and `@track_caller` may modify a trait member",
        ),
        (
            "extern \"staple-intrinsic\" {\n    @track_caller\n    f: I32 -> I32\n}\n",
            "only `@doc` may modify a declaration member",
        ),
    ] {
        let diagnostics = resolve_result(source).expect_err(source);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(message)),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn allows_local_trait_implementations_for_products() {
    let source = concat!(
        "trait ProductDefault T { product_default: () -> T }\n",
        "impl ProductDefault (I32, Bool) { product_default = () => (7, True) }\n",
        "impl ProductDefault () { product_default = () => () }\n",
        "let pair: (I32, Bool) = product_default ()\n",
        "let unit: () = product_default ()\n",
        "let (number, condition) = pair\n",
        "let answer: I32 = when { condition => number, else => 0 }\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("local trait implementations for products should generate LLVM");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn enforces_trait_implementation_orphan_rules_without_a_manifest() {
    for source in [
        "impl Default (I32, String) { default = () => (0, \"\") }\n",
        "impl Default () { default = () => () }\n",
        concat!(
            "type Local wrap I32\n",
            "impl Default (Local, I32) { default = () => (Local 0, 0) }\n",
        ),
        concat!(
            "use std.shared.(Shared)\n",
            "type Local wrap I32\n",
            "impl Default Shared<Local> { default = () => loop {} }\n",
        ),
        concat!(
            "type LocalAlias = I32\n",
            "impl Default LocalAlias { default = () => 0 }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics(
                "external traits for external top-level types must be rejected",
            );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("cannot implement an external trait for external types")
        }));
    }

    type_check(concat!(
        "type Local wrap I32\n",
        "impl Default Local { default = () => Local 0 }\n",
        "impl !Copy Local {}\n",
        "let value: Local = default ()\n",
    ));
    type_check(concat!(
        "type Generic<T> wrap T\n",
        "impl Default Generic<I32> { default = () => Generic 0 }\n",
        "let value: Generic<I32> = default ()\n",
    ));
    type_check(concat!(
        "trait LocalTrait T { value: () -> T }\n",
        "impl LocalTrait I32 { value = () => 0 }\n",
        "let value: I32 = LocalTrait.value ()\n",
    ));
}

#[test]
fn rejects_default_for_products_without_explicit_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: (I32, Bool) = default ()\n"))
        .expect_err_diagnostics("products have no implicit Default implementation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation"))
    );
}

#[test]
fn rejects_default_for_the_empty_product_without_an_explicit_implementation() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: () = default ()\n"))
        .expect_err_diagnostics("the empty product has no implicit Default implementation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation"))
    );
}

#[test]
fn resolves_names_in_function_parameters() {
    let source = "def first: _ -> I32 = (a: I32, b: I32) => a\nfirst (1, 2)\n";
    let module = resolve(source);

    assert_eq!(module.syntax().text(), source);
    assert!(
        module
            .functions()
            .iter()
            .any(|function| function.name == "first")
    );
}

#[test]
fn compiler_diagnostics_report_line_and_column() {
    let syntax = parse("let value = 1\nmissing\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("unknown name should not resolve");

    assert_eq!(
        diagnostics[0].to_string(),
        "unknown name `missing` at line 2, column 1"
    );
}

#[test]
fn infers_and_checks_function_return_types() {
    let module = type_check(concat!(
        "let first = (a: I32, b: I32) => a\n",
        "let second = (a: I32, b: I32) => I32 :: b\n",
        "first (1, second (3, 2))\n",
    ));

    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "first" | "second"))
    {
        let function_type = module
            .type_of_function(function.id)
            .expect("function should have a checked type");
        assert_eq!(*function_type.result, CheckedType::I32);
    }
}

#[test]
fn checks_type_ascriptions_and_contextually_types_functions() {
    type_check(concat!(
        "let small = I8 :: 42\n",
        "let identity = I32 -> I32 :: (value => value)\n",
        "let result: I32 = identity 42\n",
        "let left: I8 = 1\nlet right: I8 = 2\nlet sum = I8 :: left + right\n",
        "let applied = I32 :: identity 1 + identity 2\n",
        "let inferred: I64 = _ :: Convert.convert small\n",
        "let inferred_function: I32 -> I32 = _ -> I32 :: (value => value)\n",
        "let inferred_pair: (I32, String) = (I32, _) :: (1, \"text\")\n",
    ));

    let module = resolve("let invalid = I32 :: \"text\"\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("an expression must satisfy its asserted type");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `I32`, found `String`")
    }));
}

#[test]
fn infers_and_lowers_nominal_sums_with_propagation() {
    let module = type_check(concat!(
        "pub type Ok<T> wrap pub T\n",
        "pub type IOError wrap pub String\n",
        "def read: String -> Ok<String> | IOError = path => Ok(path)\n",
        "def parse = (path: String) => { let ?Ok(file) = read(path); Ok(file) }\n",
        "parse \"input\"\n",
    ));
    let parse = module
        .functions()
        .iter()
        .find(|function| function.name == "parse")
        .expect("parse function");
    let result = &module
        .type_of_function(parse.id)
        .expect("parse type")
        .result;
    let CheckedType::Sum(sum) = result.as_ref() else {
        panic!("expected inferred sum, found {result}");
    };
    assert_eq!(sum.alternatives.len(), 2);

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("sum propagation should generate LLVM");
    assert!(llvm.contains("propagate.ok"));
    assert!(llvm.contains("propagate.return"));
}

#[test]
fn exhaustively_matches_sum_values_and_destructures_payloads() {
    let module = type_check(concat!(
        "pub type IOError wrap pub String\n",
        "def choose = result: Ok<I32> | IOError => match result {\n",
        "  Ok value => value,\n",
        "  IOError _ => 0,\n",
        "}\n",
        "choose (Ok 7)\n",
    ));
    let choose = module
        .functions()
        .iter()
        .find(|function| function.name == "choose")
        .expect("choose function");
    assert_eq!(
        *module
            .type_of_function(choose.id)
            .expect("choose type")
            .result,
        CheckedType::I32
    );
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("match should generate LLVM");
    assert!(llvm.contains("match.tag"));
    assert!(llvm.contains("match.tag.matches"));
    assert!(llvm.contains("match.value"));
}

#[test]
fn matches_singletons_and_joins_nominal_arm_results() {
    let module = type_check(concat!(
        "pub type IOError wrap pub String\n",
        "def choose = value: Bool => match value {\n",
        "  True => Ok(1),\n",
        "  False => IOError(\"no\"),\n",
        "}\n",
        "choose True\n",
    ));
    let choose = module
        .functions()
        .iter()
        .find(|function| function.name == "choose")
        .expect("choose function");
    let CheckedType::Sum(sum) = module
        .type_of_function(choose.id)
        .expect("choose type")
        .result
        .as_ref()
    else {
        panic!("expected inferred sum result");
    };
    assert_eq!(sum.alternatives.len(), 2);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("singleton match should generate LLVM");
}

#[test]
fn supports_match_catch_alls_wildcard_parameters_and_returning_arms() {
    let module = type_check(concat!(
        "pub type IOError wrap pub String\n",
        "def preserve: (Ok<I32> | IOError) -> Ok<I32> | IOError = result => match result {\n",
        "  Ok value => Ok(value),\n",
        "  other => other,\n",
        "}\n",
        "def discard: I32 -> () = _ => { let _ = 1; () }\n",
        "def number = value: Bool => match value {\n",
        "  True() => { return 1 },\n",
        "  False() => { return 2 },\n",
        "}\n",
        "def subject_returns = () => match { return 3 } {\n",
        "  True() => 1,\n",
        "  False() => 2,\n",
        "}\n",
        "preserve (Ok 1)\n",
        "discard 1\n",
        "number False\n",
        "subject_returns ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("catch-all and returning matches should generate LLVM");
}

#[test]
fn matches_values_of_any_runtime_type() {
    let module = type_check(concat!(
        "pub type Wrapped wrap pub I32\n",
        "def integer = value: I32 => match value { number => number }\n",
        "def float = value: F64 => match value { _: F64 => value }\n",
        "def identity = value: I32 => value\n",
        "def function = value: (I32 -> I32) => match value { callable => callable 4 }\n",
        "def nominal = value: Wrapped => match value { Wrapped number => number }\n",
        "def generic: <T> move T -> T = <T> move value => match value { same: T => same }\n",
        "integer 1\n",
        "float 1.5\n",
        "function identity\n",
        "nominal (Wrapped 2)\n",
        "generic 3\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("matches over arbitrary runtime values should generate LLVM");
}

#[test]
fn rejects_invalid_match_patterns_and_coverage() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid = value: I32 => match value {\n",
            "  text: String => 0,\n",
            "  _ => 1,\n",
            "}\n",
        )))
        .expect_err_diagnostics("an incompatible typed pattern should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `String`, found `I32`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "match 1 {\n",
            "  first => first,\n",
            "  second => second,\n",
            "}\n",
        )))
        .expect_err_diagnostics("an arm after a scalar catch-all should be unreachable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unreachable match arm"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub type IOError wrap pub String\n",
            "def invalid = result: Ok<I32> | IOError => match result { Ok value => value, }\n",
            "invalid (Ok 1)\n",
        )))
        .expect_err_diagnostics("non-exhaustive matches should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("non-exhaustive match")
            && diagnostic.message.contains("IOError")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub type IOError wrap pub String\n",
            "def invalid = result: Ok<I32> | IOError => match result {\n",
            "  Ok value => value,\n",
            "  Ok other => other,\n",
            "  _ => 0,\n",
            "  IOError _ => 1,\n",
            "}\n",
            "invalid (Ok 1)\n",
        )))
        .expect_err_diagnostics("unreachable match arms should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("unreachable match arm") })
    );
}

#[test]
fn exhaustively_matches_product_combinations() {
    let module = type_check(concat!(
        "def same = (left: Bool, right: Bool) => match (left, right) {\n",
        "  (True(), True()) => True,\n",
        "  (False(), False()) => True,\n",
        "  _ => False,\n",
        "}\n",
        "def table = (left: Bool, right: Bool) => match (left, right) {\n",
        "  (True, True) => True,\n",
        "  (True, False) => False,\n",
        "  (False, True) => False,\n",
        "  (False, False) => True,\n",
        "}\n",
        "same (True, False)\n",
        "table (False, False)\n",
    ));
    let same = module
        .functions()
        .iter()
        .find(|function| function.name == "same")
        .expect("same function");
    let CheckedType::Sum(result) = module
        .type_of_function(same.id)
        .expect("same type")
        .result
        .as_ref()
    else {
        panic!("expected Bool result");
    };
    assert_eq!(result.alternatives.len(), 2);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("product match should generate LLVM");
    assert!(llvm.matches("match.element").count() >= 2);
    assert!(llvm.contains("match.next"));
}

#[test]
fn checks_product_match_coverage_and_reachability() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def incomplete = (left: Bool, right: Bool) => match (left, right) {\n",
            "  (True(), True()) => True,\n",
            "  (True(), False()) => False,\n",
            "}\n",
            "incomplete (True, True)\n",
        )))
        .expect_err_diagnostics("incomplete product match should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("non-exhaustive match"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def duplicate = (left: Bool, right: Bool) => match (left, right) {\n",
            "  (True(), True()) => True,\n",
            "  (True(), True()) => False,\n",
            "  _ => False,\n",
            "}\n",
            "duplicate (True, True)\n",
        )))
        .expect_err_diagnostics("duplicate product arm should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unreachable match arm"))
    );
}

#[test]
fn injects_and_widens_sum_values() {
    let module = type_check(concat!(
        "pub type IOError wrap pub String\n",
        "pub type ParseError wrap pub String\n",
        "type Result<T> = Ok<T> | IOError\n",
        "def small: () -> Ok<I32> | IOError = () => Ok(1)\n",
        "let reordered: IOError | Ok<I32> = small()\n",
        "let aliased: Result<I32> = reordered\n",
        "let duplicate: Ok<I32> | Ok<I32> = Ok(3)\n",
        "let pair: (Result<I32>, I32) = (aliased, 2)\n",
        "def captured = () => { let local: Result<I32> = pair.0; let inner = () => local; inner() }\n",
        "def wide: () -> Ok<I32> | IOError | ParseError = () => small()\n",
        "captured()\n",
        "wide()\n",
    ));
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    generator
        .compile_root(&lower(&module))
        .expect("sum injection and widening should generate valid LLVM");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow the Unix epoch")
        .as_nanos();
    let output = std::env::temp_dir().join(format!("staple-compiler-sum-test-{nonce}.o"));
    generator
        .emit_object(&lower_whole(&module), &output, None)
        .expect("sum values should emit a native object");
    std::fs::remove_file(output).expect("temporary sum object should be removable");
}

#[test]
fn propagates_each_residual_variant_and_joins_explicit_returns() {
    let module = type_check(concat!(
        "pub type IOError wrap pub String\n",
        "pub type ParseError wrap pub String\n",
        "def fail: () -> Ok<I32> | IOError = () => IOError(\"io\")\n",
        "def parse = () => { let ?Ok(value) = fail(); return ParseError(\"parse\"); }\n",
        "parse()\n",
    ));
    let parse = module
        .functions()
        .iter()
        .find(|function| function.name == "parse")
        .expect("parse function");
    let CheckedType::Sum(sum) = module
        .type_of_function(parse.id)
        .expect("parse type")
        .result
        .as_ref()
    else {
        panic!("expected inferred sum");
    };
    assert_eq!(sum.alternatives.len(), 2);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("residual-only propagation widening should generate LLVM");
}

#[test]
fn supports_arbitrary_sized_sum_alternatives_and_typed_matches() {
    let module = type_check(concat!(
        "def select = value: I32 | String => match value {\n",
        "  number: I32 => number,\n",
        "  text: String => 0,\n",
        "}\n",
        "let integer: I32 | String = 41\n",
        "let text: I32 | String = \"hello\"\n",
        "let pair: (I32, I32) | F64 = (1, 2)\n",
        "def identity = value: I32 => value\n",
        "let function: (I32 -> I32) | String = identity\n",
        "let applied: Ok<I32> | Ok<String> = Ok(3)\n",
        "def small: () -> I32 | String = () => 7\n",
        "let widened: I32 | String | F64 = small()\n",
        "def generic: <T> move T -> T | String = <T> move value => value\n",
        "generic 9\n",
        "select(integer)\n",
        "select(text)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("arbitrary sized sum alternatives should lower to LLVM");
}

#[test]
fn rejects_unsized_sum_alternatives_and_ambiguous_nominal_patterns() {
    for (source, expected) in [
        (
            "def value: <T where ?Sized T> () -> T | String = <T where ?Sized T> () => ()\n",
            "must be a sized type",
        ),
        (
            concat!(
                "def inspect = value: Ok<I32> | Ok<String> => match value {\n",
                "  Ok(payload) => 0,\n",
                "  _ => 1,\n",
                "}\n",
            ),
            "selects more than one alternative",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("invalid sum should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn postfix_try_requires_a_function_and_a_result_that_contains_the_residual() {
    let diagnostics = resolve_result("let value = Ok(1) ?\n")
        .expect_err("`?` outside a function should be rejected");
    assert!(!diagnostics.is_empty());

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def read: () -> Ok<I32> | String = () => Ok(1)\n",
            "def invalid: () -> Ok<I32> = () => { let value = read () ?; Ok(value) }\n",
        )))
        .expect_err_diagnostics("the residual must be contained in the result type");
    assert!(!diagnostics.is_empty(), "{diagnostics:?}");

    TypeChecker::new()
        .check(resolve(concat!(
            "def read: () -> Ok<I32> | String = () => Ok(1)\n",
            "def forwards = () => { let value = read () ?; Ok(value) }\n",
            "def explicit: () -> Ok<I32> | String = () => { let value = read () ?; Ok(value) }\n",
        )))
        .expect("`?` should widen the residual into the result type");
}

#[test]
fn rejects_invalid_propagation_and_sum_ffi() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub type IOError wrap pub String\n",
            "def invalid = () => { let ?Ok(value) = Ok(1); Ok(value) }\n",
        )))
        .expect_err_diagnostics("propagation requires a sum");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`let ?` binding requires a sum value")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub type IOError wrap pub String\n",
            "def read: () -> Ok<I32> | IOError = () => Ok(1)\n",
            "def invalid: () -> Ok<I32> = () => { let ?Ok(value) = read(); Ok(value) }\n",
        )))
        .expect_err_diagnostics("explicit result should contain propagated variants");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("propagated variant `IOError` is not contained")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub type IOError wrap pub String\n",
            "extern \"c\" { invalid: () -> Ok<I32> | IOError }\n",
        )))
        .expect_err_diagnostics("sum ABI should remain internal");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("external binding types cannot contain sums")
    }));
}

#[test]
fn rejects_propagation_outside_functions_and_non_nominal_roots() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "pub type IOError wrap pub String\n",
                "let ?Ok(value) = Ok(1)\n",
                "def invalid = () => { let ?(Ok(value), other) = (Ok(1), 2); Ok(value) }\n",
            ),
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("invalid propagation contexts should not resolve");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("only allowed inside a function")
    }));
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("requires a nominal pattern") })
    );
}

#[test]
fn infers_return_types_through_forward_function_references() {
    let module = type_check(concat!(
        "def first = () => second ()\n",
        "def second = () => 42\n",
        "first ()\n",
    ));

    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "first" | "second"))
    {
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("function should have a checked type")
                .result,
            CheckedType::I32,
        );
    }
}

#[test]
fn rejects_an_immediate_read_before_a_def_initializer() {
    let syntax = parse("value\ndef value = 10\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("an immediate forward read must fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("read before it is initialized"))
    );
}

#[test]
fn permits_deferred_mutual_recursion_with_selective_state() {
    let module = type_check(concat!(
        "def f: () -> I32 = () => g ()\n",
        "def g: () -> I32 = () => f ()\n",
        "f ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("capturing a forward binding must remain legal");
    assert!(llvm.contains(".g_state"));
    assert!(llvm.contains("binding.uninitialized"));
    assert!(!llvm.contains(".f_state"));
}

#[test]
fn captures_potentially_unsafe_local_defs_by_binding_cell() {
    let module = type_check(concat!(
        "def outer: () -> I32 = () => {\n",
        "  def f: () -> I32 = () => g ()\n",
        "  def g: () -> I32 = () => f ()\n",
        "  f ()\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("unsafe local captures should share a binding cell");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("binding.uninitialized"));
}

#[test]
fn applies_initialization_state_to_recursive_local_generics() {
    let module = type_check(concat!(
        "def outer: () -> I32 = () => {\n",
        "  def recur: <T> T -> T = <T> value => recur value\n",
        "  recur 1\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generic functions use the same initialization state model");
    assert!(llvm.contains("binding.state.cell"));
    assert!(llvm.contains("binding.uninitialized"));
}

#[test]
fn does_not_add_state_metadata_to_safe_bindings() {
    let module = type_check("def answer = 42\nanswer\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("safe bindings should compile without state metadata");
    assert!(!llvm.contains(".answer_state"));
}

#[test]
fn rejects_an_incorrect_function_result_type() {
    let module = resolve("use std.cinterop.*\nlet answer = () => CString :: 42\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("incorrect return type should fail");

    assert!(
        diagnostics[0]
            .message
            .contains("expected `CString`, found `I32`")
    );
}

#[test]
fn rejects_incorrect_call_arguments() {
    let module = resolve("let identity = (value: I32) => value\nidentity (\"wrong\")\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("incorrect argument type should fail");

    assert!(
        diagnostics[0]
            .message
            .contains("expected `I32`, found `String`")
    );
}

#[test]
fn treats_singleton_products_as_their_element() {
    let module = type_check(concat!(
        "let answer: (I32) = 42\n",
        "let identity = (value: I32) => value\n",
        "identity (answer)\n",
    ));
    let function = module
        .functions()
        .iter()
        .find(|function| function.name == "identity")
        .expect("identity should resolve");
    let function_type = module
        .type_of_function(function.id)
        .expect("identity should have a type");

    assert_eq!(*function_type.parameter, CheckedType::I32);

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("singleton products should compile as their element");
    assert!(llvm.contains("define i32 @identity(ptr %0, i32 %value)"));
    assert!(!llvm.contains("define i32 @identity(ptr %0, { i32 }"));
}

#[test]
fn destructures_nested_product_patterns() {
    let module = type_check(concat!(
        "let add_nested = (x: I32, (y: I32, z: I32)) => x\n",
        "add_nested (1, (2, 3))\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nested product pattern should compile");

    assert!(llvm.contains("define i32 @add_nested(ptr %0, i32 %x, { i32, i32 }"));
    assert!(llvm.contains("extractvalue { i32, i32 }"));
}

#[test]
fn binds_a_product_without_destructuring_it() {
    let module = type_check(concat!(
        "let sum = pair: (I32, I32) => pair.0\n",
        "sum (1, 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a product binding pattern should compile");

    assert!(llvm.contains("define i32 @sum(ptr %0, i32 %1, i32 %2)"));
    assert!(llvm.contains("extractvalue { i32, i32 }"));
}

#[test]
fn binds_whole_copy_values_while_destructuring_them() {
    let module = type_check(concat!(
        "def local = () => { let mut point: (I32, I32)@(x, y) = (20, 22); point.0 = 21; point.0 + y }\n",
        "def parameter = args@(_: I32, _: I32) => args.0 + args.1\n",
        "def chained = outer@inner@(left: I32, right: I32) => outer.0 + inner.1 + left + right\n",
        "def captured = pair@(left: I32, right: I32) => { let read = () => pair.0; read() + left + right }\n",
        "local()\n",
        "parameter (20, 22)\n",
        "chained (1, 2)\n",
        "captured (10, 16)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("at-patterns over Copy products should compile");

    assert!(llvm.contains("define i32 @parameter(ptr %0, i32 %1, i32 %2)"));
    assert!(!llvm.contains("define i32 @parameter(ptr %0, { i32, i32 }"));
    assert!(llvm.contains("%args"));
}

#[test]
fn at_patterns_are_structural_in_matches_and_propagation() {
    let module = type_check(concat!(
        "pub type IOError wrap pub String\n",
        "def read: () -> Ok<I32> | IOError = () => Ok(42)\n",
        "def choose = pair: (Bool, I32) => match pair {\n",
        "  whole@(True(), value) => whole.1 + value,\n",
        "  _ => 0,\n",
        "}\n",
        "def parse = () => { let ?result@Ok(value) = read(); result }\n",
        "choose (True, 1)\n",
        "parse()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("at-patterns should preserve match and propagation lowering");
    assert!(llvm.contains("match.tag"));
    assert!(llvm.contains("propagate.ok"));
}

#[test]
fn at_patterns_require_copy_runtime_values_and_honor_copy_bounds() {
    type_check("def retain: <T where Copy T> T -> T = <T where Copy T> value@_ => value\n");

    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid: <T> T -> T = <T> value@_ => value\n"))
        .expect_err_diagnostics("an unconstrained generic at-pattern should not be Copy");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("an `@` pattern requires a Copy value")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.CString\n",
            "def invalid = value: CString => { let whole@_ = value; () }\n",
        )))
        .expect_err_diagnostics("a move-only concrete at-pattern should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("an `@` pattern requires a Copy value")
    }));
}

#[test]
fn compile_time_at_patterns_bind_whole_syntax_values() {
    let module = type_check(concat!(
        "macro keep = whole@value: Expr => whole\n",
        "let answer: I32 = keep 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("compile-time at-patterns should clone syntax values");
}

#[test]
fn type_checks_transparent_aliases() {
    type_check("type number = I32\nlet answer: number = 42\n");
}

#[test]
fn operator_traits_and_methods_require_explicit_imports() {
    for (trait_name, method, arguments) in [
        ("Add", "add", "(1; 2)"),
        ("Subtract", "subtract", "(4; 3)"),
        ("Multiply", "multiply", "(2; 3)"),
        ("Divide", "divide", "(8; 2)"),
        ("Neg", "negate", "operand"),
        ("Not", "not", "flag"),
        ("BitAnd", "bit_and", "(1; 2)"),
        ("BitOr", "bit_or", "(1; 2)"),
        ("BitXor", "bit_xor", "(1; 2)"),
        ("BitNot", "bit_not", "operand"),
        ("ShiftLeft", "shift_left", "(1; 2)"),
        ("ShiftRight", "shift_right", "(1; 2)"),
    ] {
        for usage in [
            format!("let answer = {trait_name}.{method} {arguments}\n"),
            format!("let answer = {method} {arguments}\n"),
            format!(
                "def identity: <T where Copy T, {trait_name} T> T -> T = <T where Copy T, {trait_name} T> value => value\n"
            ),
        ] {
            let usage = format!("let operand: I32 = 1\nlet flag: Bool = True\n{usage}");
            resolve_result(&usage).expect_err_diagnostics("operator names must require imports");
            let source = format!("use std.ops.({trait_name})\n{usage}");
            TypeChecker::new()
                .check(resolve(&source))
                .unwrap_or_else(|diagnostics| {
                    panic!("explicit import should type-check `{source}`: {diagnostics:?}")
                });
        }
    }
}

#[test]
fn arithmetic_operators_work_without_trait_imports() {
    let module = type_check(concat!(
        "let sum = 1 + 2\n",
        "let difference = 4 - 3\n",
        "let product = 2 * 3\n",
        "let quotient = 8 / 2\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("arithmetic without trait imports should compile");

    assert!(llvm.contains("add i32"));
    assert!(llvm.contains("sub i32"));
    assert!(llvm.contains("mul i32"));
    assert!(llvm.contains("sdiv i32"));
    assert!(!llvm.contains("declare i32 @__i32_add"));
}

#[test]
fn comparison_operators_work_without_trait_imports() {
    let module = type_check(concat!(
        "let equal = 1 == 1\n",
        "let not_equal = 1 != 2\n",
        "let less = 1 < 2\n",
        "let less_equal = 1 <= 2\n",
        "let greater = 2 > 1\n",
        "let greater_equal = 2 >= 1\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("comparison without trait imports should compile");

    for predicate in ["icmp eq", "icmp ne", "icmp slt", "icmp sgt"] {
        assert!(
            llvm.contains(predicate),
            "missing `{predicate}` in LLVM without a `std.cmp` import"
        );
    }
}

#[test]
fn bitwise_traits_lower_to_intrinsics_for_every_integer_type() {
    let types = [
        "I8", "I16", "I32", "I64", "U8", "U16", "U32", "U64", "ISize", "USize",
    ];
    let mut source =
        String::from("use std.ops.(BitAnd, BitOr, BitXor, BitNot, ShiftLeft, ShiftRight)\n");
    for ty in types {
        let lower = ty.to_lowercase();
        for (op, method) in [
            ("and", "BitAnd.bit_and (left; right)"),
            ("or", "BitOr.bit_or (left; right)"),
            ("xor", "BitXor.bit_xor (left; right)"),
            ("shift_left", "ShiftLeft.shift_left (left; right)"),
            ("shift_right", "ShiftRight.shift_right (left; right)"),
        ] {
            source.push_str(&format!(
                "let {op}_{lower} = (left: {ty}, right: {ty}) => {method}\n"
            ));
        }
        source.push_str(&format!(
            "let not_{lower} = (value: {ty}) => BitNot.bit_not value\n"
        ));
        source.push_str(&format!("let call_{lower} = and_{lower} (6, 3)\n"));
        source.push_str(&format!("let call_not_{lower} = not_{lower} 0\n"));
        source.push_str(&format!(
            "let call_right_{lower} = shift_right_{lower} (8, 1)\n"
        ));
    }
    let module = type_check(&source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("bitwise operations should compile");

    for ty in types {
        let lower = ty.to_lowercase();
        for operation in ["and", "or", "xor", "shift_left", "shift_right"] {
            let name = format!("{lower}.{operation}");
            assert!(llvm.contains(&name), "missing `{name}` in LLVM");
        }
        let name = format!("{lower}.not");
        assert!(llvm.contains(&name), "missing `{name}` in LLVM");
    }
    assert!(
        llvm.contains("ashr i32"),
        "signed shifts must be arithmetic"
    );
    assert!(llvm.contains("lshr i32"), "unsigned shifts must be logical");
    assert!(!llvm.contains("declare i32 @__i32_and"));
}

#[test]
fn bitwise_traits_reject_unsupported_types() {
    for ty in ["F32", "F64", "Bool"] {
        let diagnostics = TypeChecker::new()
            .check(resolve(&format!(
                "use std.ops.(BitAnd, BitNot, ShiftLeft)\n\
                 let unsupported_and = (left: {ty}, right: {ty}) => BitAnd.bit_and (left; right)\n\
                 let unsupported_not = (value: {ty}) => BitNot.bit_not value\n\
                 let unsupported_shift = (value: {ty}) => ShiftLeft.shift_left (value; value)\n"
            )))
            .expect_err_diagnostics("bitwise operations must reject unsupported types");
        assert!(!diagnostics.is_empty());
    }
}

#[test]
fn compares_all_standard_library_integer_types() {
    let module = type_check(concat!(
        "use std.cmp.(Eq, PartialOrd)\n",
        "let equal: Bool = 1 == 1\n",
        "let not_equal: Bool = 1 != 2\n",
        "let less: Bool = 1 < 2\n",
        "let less_equal: Bool = 1 <= 2\n",
        "let greater: Bool = 2 > 1\n",
        "let greater_equal: Bool = 2 >= 1\n",
        "def same: <T where Copy T, Eq T> T -> T -> Bool = <T where Copy T, Eq T> left => right => left == right\n",
        "def before: <T where Copy T, PartialOrd T> T -> T -> Bool = <T where Copy T, PartialOrd T> left => right => left < right\n",
        "let generic_equal: Bool = same 1 1\n",
        "let generic_order: Bool = before 1 2\n",
        "def i8 = (x: I8, y: I8) => x < y\n",
        "def i16 = (x: I16, y: I16) => x < y\n",
        "def i64 = (x: I64, y: I64) => x < y\n",
        "def u8 = (x: U8, y: U8) => x < y\n",
        "def u16 = (x: U16, y: U16) => x < y\n",
        "def u32 = (x: U32, y: U32) => x < y\n",
        "def u64 = (x: U64, y: U64) => x < y\n",
        "def isize = (x: ISize, y: ISize) => x < y\n",
        "def usize = (x: USize, y: USize) => x < y\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("integer comparisons should compile");

    for predicate in [
        "icmp eq", "icmp ne", "icmp slt", "icmp sgt", "icmp ult", "icmp ugt",
    ] {
        assert!(llvm.contains(predicate), "missing `{predicate}` in LLVM");
    }
    assert!(llvm.contains("bool.tag"));
}

#[test]
fn supports_contextual_float_literals_arithmetic_and_partial_ordering() {
    let module = type_check(concat!(
        "use std.cmp.(PartialOrd, Ord, Ordering)\n",
        "extern \"c\" { scale_float: F64 -> F64 }\n",
        "def single = () => F32 :: { let a: F32 = 1.5; let b: F32 = .5; (a + b) * b - a / b; }\n",
        "def double = () => F64 :: { let a: F64 = 1e3; let b: F64 = 2.; (a + b) / b; }\n",
        "def defaulted = () => 1.25\n",
        "let less: Bool = 1.0 < 2.0\n",
        "let equal: Bool = 2.0 == 2.0\n",
        "let scaled: F64 = scale_float 2.0\n",
        "let unordered: Option<Ordering> = PartialOrd.partial_cmp ((0.0 / 0.0); 1.0)\n",
        "let ordered_less: Ordering = Ord.cmp (1; 2)\n",
        "let ordered_equal: Ordering = Ord.cmp (2; 2)\n",
        "let ordered_greater: Ordering = Ord.cmp (3; 2)\n",
        "single ()\n",
    ));
    let defaulted = module
        .functions()
        .iter()
        .find(|function| function.name == "defaulted")
        .unwrap();
    assert_eq!(
        *module.type_of_function(defaulted.id).unwrap().result,
        CheckedType::F64
    );

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .expect("float operations should compile");
    for instruction in [
        "fadd float",
        "fmul float",
        "fsub float",
        "fdiv float",
        "fadd double",
        "fdiv double",
        "fcmp olt",
        "fcmp oeq",
        "fcmp une",
    ] {
        assert!(
            llvm.contains(instruction),
            "missing `{instruction}` in LLVM"
        );
    }
    assert!(llvm.contains("declare double @scale_float(double)"));
}

#[test]
fn rejects_invalid_float_contexts_and_float_ord() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let value: F32 = 1e100\n"))
        .expect_err_diagnostics("overflowing F32 literal should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not fit in `F32`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("let value: F32 = 1\n"))
        .expect_err_diagnostics("integer literals should not become floats");
    assert!(!diagnostics.is_empty());

    let diagnostics = TypeChecker::new()
        .check(resolve("let value = 1e+\n"))
        .expect_err_diagnostics("an incomplete exponent should fail type checking");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("float literal"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("use std.cmp.(Ord, Ordering)\ndef compare: <T where Copy T, Ord T> T -> T -> Ordering = <T where Copy T, Ord T> left => right => Ord.cmp (left; right)\nlet invalid = compare 1.0 2.0\n"))
        .expect_err_diagnostics("floats should not implement Ord");
    assert!(!diagnostics.is_empty());
}

#[test]
fn requires_only_the_ordering_methods() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.cmp.PartialOrd\ntype Text wrap String\nimpl PartialOrd Text {}\n",
        ))
        .expect_err_diagnostics("partial_cmp is required");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing member `partial_cmp`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.cmp.Ord\ntype Text wrap String\nimpl Ord Text {}\n",
        ))
        .expect_err_diagnostics("cmp is required");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing member `cmp`"))
    );
}

#[test]
fn requires_only_the_equality_method() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.cmp.Eq\ntype Text wrap String\nimpl Eq Text {}\n",
        ))
        .expect_err_diagnostics("eq is required");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing member `eq`"))
    );

    let module = type_check(concat!(
        "use std.cmp.Eq\n",
        "type Point wrap (x: I32, y: I32)\n",
        "impl Eq Point { eq = (left; right) => left.x == right.x && left.y == right.y }\n",
        "let a: Point = Point (x: 1, y: 2)\n",
        "let b: Point = Point (x: 1, y: 2)\n",
        "let c: Point = Point (x: 3, y: 4)\n",
        "let same: Bool = a == b\n",
        "let different: Bool = a != c\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("the default `ne` should back `!=`");
}

#[test]
fn compares_library_defined_bool_values() {
    let module = type_check(concat!(
        "let yes: Bool = True\n",
        "let no: Bool = False\n",
        "let true_equals_true: Bool = yes == yes\n",
        "let false_equals_false: Bool = no == no\n",
        "let true_differs_false: Bool = yes != no\n",
        "let false_differs_true: Bool = no != yes\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("Bool equality should dispatch through its standard-library implementation");
    assert!(llvm.contains("match.tag.matches"));
}

#[test]
fn lowercase_i32_is_an_ordinary_unresolved_type_name() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let value: i32 = 1\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("lowercase i32 should not name the builtin");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `i32`")
    );
}

#[test]
fn bool_is_an_auto_loaded_standard_library_type() {
    let module = type_check(concat!(
        "let yes: Bool = True\n",
        "let no: Bool = False\n",
        "def require_true = (value: Bool) => { let ?True() = value; True }\n",
        "require_true(yes)\n",
    ));
    let require_true = module
        .functions()
        .iter()
        .find(|function| function.name == "require_true")
        .expect("require_true function");
    assert!(matches!(
        module
            .type_of_function(require_true.id)
            .expect("require_true type")
            .result
            .as_ref(),
        CheckedType::Sum(sum) if sum.alternatives.len() == 2
    ));

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("library-defined Bool should use ordinary sum lowering");
}

#[test]
fn lowercase_bool_is_an_ordinary_unresolved_type_name() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("def predicate: bool\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("lowercase bool should not name the builtin");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `bool`")
    );
}

#[test]
fn cinterop_types_require_an_explicit_import() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("def text: CString\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("CString should not be in the prelude");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `CString`")
    );

    type_check(concat!(
        "use std.cinterop.*\nuse std.ptr.*\n",
        "def character: CChar\n",
        "def pointer: RawPtr<CChar>\n",
        "def text: CString\n",
    ));
}

#[test]
fn raw_pointer_types_require_std_ptr_imports() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for imports in ["", "use std.cinterop.*\n"] {
        let source = format!(
            "{imports}def pointer: RawPtr<I32>\ndef opaque: RawOpaquePtr\ndef mutable: RawMutPtr<I32>\ndef opaque_mutable: RawOpaqueMutPtr\n"
        );
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, root)
            .expect("source should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("raw pointer types require std.ptr imports");
        for name in ["RawPtr", "RawOpaquePtr", "RawMutPtr", "RawOpaqueMutPtr"] {
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| { diagnostic.message == format!("unknown type `{name}`") }),
                "{diagnostics:?}"
            );
        }
    }
    type_check(concat!(
        "use std.ptr.(RawPtr, RawOpaquePtr, RawMutPtr, RawOpaqueMutPtr)\n",
        "def pointer: RawPtr<I32> -> RawPtr<I32> = value => value\n",
        "def opaque: RawOpaquePtr -> RawOpaquePtr = value => value\n",
        "def mutable: RawMutPtr<I32> -> RawMutPtr<I32> = value => value\n",
        "def opaque_mutable: RawOpaqueMutPtr -> RawOpaqueMutPtr = value => value\n",
    ));
}

#[test]
fn syntax_types_and_quote_require_an_explicit_import() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("type Captured = SyntaxNode\n", root)
        .expect("source should load without importing syntax names");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("syntax types should not be in the core prelude");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `SyntaxNode`"),
        "{diagnostics:#?}",
    );

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro identity: Expr -> Syntax = value => quote { $value }\nidentity 1\n",
            root,
        )
        .expect("quote syntax should parse before import resolution");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("quote should require an explicit import");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "`quote` requires an explicit import from `std.syntax`"
    }));

    resolve(concat!(
        "use std.syntax.(parse_quote, Expr)\n",
        "macro identity: Expr -> Expr = value => parse_quote { $value }\n",
        "let result: I32 = identity 42\n",
    ));

    resolve(concat!(
        "use std.syntax.(Syntax, Expr)\n",
        "macro capture: Syntax -> Syntax = value => std.syntax.quote { $value }\n",
        "macro identity: Expr -> Expr = value => std.syntax.parse_quote { $value }\n",
        "let result: I32 = identity 42\n",
    ));

    resolve(concat!(
        "use std.syntax\n",
        "use std.syntax.(Syntax, Expr)\n",
        "macro capture: Syntax -> Syntax = value => syntax.quote { $value }\n",
        "macro identity: Expr -> Expr = value => syntax.parse_quote { $value }\n",
        "let result: I32 = identity 42\n",
    ));
}

#[test]
fn c_pointer_preserves_its_pointee_type() {
    let module = resolve(concat!(
        "use std.cinterop.*\nuse std.ptr.*\n",
        "extern \"c\" { consume: (RawPtr<I32>) -> I32 }\n",
        "consume (c_string \"wrong pointee\")\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("CString should only coerce to RawPtr CChar");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `RawPtr<I32>`, found `CString`")
    }));
}

#[test]
fn generic_opaque_arguments_are_part_of_type_identity() {
    let module = resolve(concat!(
        "use std.shared.(Shared)\n",
        "type Handle<T> opaque\n",
        "def invalid: Shared<Handle<I32>> -> Shared<Handle<String>> = value => value\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("opaque applications with different arguments must differ");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `Shared<Handle<String>>`, found `Shared<Handle<I32>>`")
    }));
}

#[test]
fn string_literals_have_the_canonical_string_type() {
    let module = type_check("\"hello\"\n");
    let item = &module.syntax().items[0];
    let Item::Expression(expression) = item else {
        panic!("expected expression item");
    };

    assert_eq!(
        module.type_of_expression(expression.syntax().id),
        Some(&CheckedType::String)
    );
}

#[test]
fn buffer_intrinsics_type_check_and_compile() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "use std.buffer.Buffer\nuse std.shared_buffer.SharedBuffer\n",
        "def exercise: () -> () = () => {\n",
        "let mut values: Buffer<I32> = Buffer.with_capacity (USize :: 2)\n",
        "let empty_length: USize = Buffer.length values\n",
        "let capacity: USize = Buffer.capacity values\n",
        "Buffer.push values 10\n",
        "Buffer.push values 20\n",
        "let first: I32 = Buffer.get_unchecked values (USize :: 0)\n",
        "let safe_first: Option<I32> = Buffer.get values (USize :: 0)\n",
        "let replaced: Option<()> = Buffer.set values (USize :: 1) 30\n",
        "Buffer.set_unchecked values (USize :: 0) 40\n",
        "let popped: Option<I32> = Buffer.pop values\n",
        "let frozen: SharedBuffer<I32> = Buffer.into_shared_buffer values\n",
        "let frozen_length: USize = SharedBuffer.length frozen\n",
        "let safe_frozen: Option<Shared<I32>> = SharedBuffer.get_shared frozen (USize :: 0)\n",
        "()\n",
        "}\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("Buffer operations should compile");
    assert!(llvm.contains("unique.allocate"));
    assert!(llvm.contains("buffer.into_shared_buffer.allocate"));
    assert!(llvm.contains("buffer.push.slot"));
    assert!(llvm.contains("buffer.pop.option"));
    assert!(llvm.contains("shared_buffer.length"));

    let module = type_check(concat!(
        "use std.buffer.Buffer\n",
        "use std.shared_buffer.SharedBuffer\n",
        "use std.cinterop.*\n",
        "def exercise: () -> () = () => {\n",
        "let mut owned: Buffer<CString> = Buffer.with_capacity (USize :: 1)\n",
        "Buffer.push owned (c_string \"owned\")\n",
        "let frozen: SharedBuffer<CString> = Buffer.into_shared_buffer owned\n",
        "()\n",
        "}\n",
    ));
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("Buffer should own non-Default, non-Copy elements");
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::GcBufferFinalizer);
}

#[test]
fn buffer_and_list_are_move_only_and_clone_their_elements() {
    let module = type_check(concat!(
        "use std.buffer.Buffer\nuse std.shared_buffer.SharedBuffer\n",
        "type Resource wrap I32\n",
        "impl !Copy Resource {}\n",
        "impl Clone Resource { clone = Resource value => Resource value }\n",
        "def exercise: () -> () = () => {\n",
        "let mut buffer: Buffer<Resource> = Buffer.with_capacity (USize :: 4)\n",
        "Buffer.push buffer (Resource 7)\n",
        "let mut cloned_buffer: Buffer<Resource> = Clone.clone buffer\n",
        "Buffer.push cloned_buffer (Resource 8)\n",
        "let buffer_capacity: USize = Buffer.capacity cloned_buffer\n",
        "let mut list: List<Resource> = List.with_capacity (USize :: 4)\n",
        "List.push list (Resource 9)\n",
        "let mut cloned_list: List<Resource> = Clone.clone list\n",
        "List.push cloned_list (Resource 10)\n",
        "let list_capacity: USize = List.capacity cloned_list\n",
        "()\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("Buffer and List deep clones should compile");
    assert!(llvm.contains("unique.allocate"));
    assert!(llvm.contains("buffer.clone.element"));
    assert!(llvm.contains("buffer.clone.destination.length.slot"));

    for source in [
        concat!(
            "use std.buffer.Buffer\n",
            "def invalid: () -> USize = () => {\n",
            "let buffer: Buffer<I32> = Buffer.with_capacity (USize :: 1)\n",
            "let moved = buffer\n",
            "Buffer.length buffer\n",
            "}\n",
        ),
        concat!(
            "use std.buffer.Buffer\nuse std.shared_buffer.SharedBuffer\n",
            "def invalid: () -> USize = () => {\n",
            "let list: List<I32> = List.new ()\n",
            "let moved = list\n",
            "List.length list\n",
            "}\n",
        ),
        concat!(
            "use std.buffer.Buffer\nuse std.shared_buffer.SharedBuffer\n",
            "def invalid: () -> USize = () => {\n",
            "let buffer: Buffer<I32> = Buffer.with_capacity (USize :: 1)\n",
            "let frozen: SharedBuffer<I32> = Buffer.into_shared_buffer buffer\n",
            "Buffer.length buffer\n",
            "}\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("moved containers must not remain usable");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("moved"))
        );
    }

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.buffer.Buffer\n",
            "type Resource wrap I32\n",
            "impl !Copy Resource {}\n",
            "def invalid: Buffer<Resource> -> Buffer<Resource> = buffer => Clone.clone buffer\n",
        )))
        .expect_err_diagnostics("Buffer cloning should require Clone elements");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("trait implementation")
            || diagnostic.message.contains("trait bound")
    }));
}

#[test]
fn buffer_transfer_type_checks_and_compiles() {
    let module = type_check(concat!(
        "use std.buffer.Buffer\n",
        "def exercise: () -> () = () => {\n",
        "let mut source: Buffer<I32> = Buffer.with_capacity (USize :: 2)\n",
        "Buffer.push source 1\n",
        "Buffer.push source 2\n",
        "let mut destination: Buffer<I32> = Buffer.with_capacity (USize :: 5)\n",
        "Buffer.push destination 0\n",
        "Buffer.transfer source destination\n",
        "let moved_length: USize = Buffer.length destination\n",
        "let emptied_length: USize = Buffer.length source\n",
        "()\n}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("Buffer.transfer should compile");
    assert!(llvm.contains("buffer.transfer.aliased"));
    assert!(llvm.contains("buffer.transfer.insufficient_capacity"));
    assert!(llvm.contains("buffer.transfer.dest.write"));
    assert!(llvm.contains("llvm.memcpy"));
}

#[test]
fn list_grows_past_initial_capacity_and_type_checks() {
    let module = type_check(concat!(
        "def exercise: () -> () = () => {\n",
        "let mut values: List<I32> = List.new ()\n",
        "List.push values 1\n",
        "List.push values 2\n",
        "List.push values 3\n",
        "List.push values 4\n",
        "List.push values 5\n",
        "let length: USize = List.length values\n",
        "let capacity: USize = List.capacity values\n",
        "let first: Option<I32> = List.get values (USize :: 0)\n",
        "let last: I32 = List.get_unchecked values (USize :: 4)\n",
        "let replaced: Option<()> = List.set values (USize :: 4) 9\n",
        "List.set_unchecked values (USize :: 0) 8\n",
        "let popped: Option<I32> = List.pop values\n",
        "let sized: List<I32> = List.with_capacity (USize :: 10)\n",
        "()\n}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("List operations should compile");
    assert!(llvm.contains("buffer.transfer.dest.write"));
}

#[test]
fn migrated_stdlib_methods_accept_pipe_method_call_syntax() {
    // The juxtaposed companion methods take the receiver as their first slot,
    // so `receiver |> method rest…` resolves the same call as `Type.method`.
    let module = type_check(concat!(
        "def exercise: () -> () = () => {\n",
        "let mut values: List<I32> = List.new ()\n",
        "values |> push 1\n",
        "values |> push 2\n",
        "let first: Option<I32> = values |> get (USize :: 0)\n",
        "let first_unchecked: I32 = values |> get_unchecked (USize :: 0)\n",
        "let replaced: Option<()> = values |> set (USize :: 0) 3\n",
        "values |> set_unchecked (USize :: 0) 4\n",
        "()\n}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("caret method calls on migrated stdlib methods should compile");
}

#[test]
fn list_get_requires_copy_element_type() {
    let resolved = resolve(concat!(
        "use std.cinterop.*\n",
        "let mut values: List<CString> = List.new ()\n",
        "List.push values (c_string \"a\")\n",
        "let first: CString = List.get values (USize :: 0)\n",
    ));
    let result = TypeChecker::new().check(resolved);
    assert!(
        result.is_err(),
        "List.get should be rejected for a non-Copy element type"
    );
}

#[test]
fn companion_where_bound_resolves_independently_for_every_member() {
    // Regression test: a `companion<T where Bound T> Type T { ... }` with
    // two or more members used to splice the *same* `SyntaxId`s for the
    // companion's own generics into every member (a plain `.clone()`, not a
    // fresh re-parse). Any resolver pass that caches a `SyntaxId`'s resolved
    // `TypeParameterId` then had every member overwrite the same cache
    // entry, so only the last-declared member's bound resolved to the right
    // type parameter — every earlier member's `where` bound stayed
    // unresolved (referencing another member's parameter), and calling it
    // failed with "trait bound is not satisfied" even when the bound
    // plainly held.
    let module = type_check(concat!(
        "pub type Box<T> wrap pub (value: T)\n",
        "companion<T where Copy T> Box<T> {\n",
        "    pub def first: Box<T> -> T = Box value => value\n",
        "    pub def second: Box<T> -> T = Box value => value\n",
        "}\n",
        "let boxed: Box<I32> = Box (value: 5)\n",
        "let a: I32 = Box.first boxed\n",
        "let b: I32 = Box.second boxed\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("every member of a where-bounded companion should resolve its own bound");
}

#[test]
fn macro_declared_inside_a_companion_resolves_as_type_dot_macro() {
    // Regression test: a `macro` declared inside `companion<T> Type T { ... }`
    // parsed fine, but macro expansion never learned that `Type` is a
    // macro-callable namespace. Value/type/trait lookup broadcasts every
    // companion from `std.core` into every module's prelude (so
    // `List.new()` etc. need no import), but macro expansion's own,
    // separate scope-construction pass only ever broadcast macros/helpers
    // that way, never namespaces — so `Type.macro(...)` was unreachable
    // from any file that didn't import the type in an unusual, unbraced
    // form. Also exercises a companion body resolving a `use`d name from
    // its *parent* module (here, `quote`/`parse_quote` from `std.syntax`)
    // without repeating the import inside the companion itself.
    let module = type_check(concat!(
        "use std.syntax.*\n",
        "pub type Box<T> wrap pub (value: T)\n",
        "companion<T> Box<T> {\n",
        "    pub macro of = value: Expr => parse_quote { Box (value: $value) }\n",
        "}\n",
        "let boxed: Box<I32> = Box.of 5\n",
        "let value: I32 = boxed.value\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a macro declared inside a companion should resolve as Type.macro");
}

#[test]
fn dispatches_generic_implementations_of_traits_with_associated_types() {
    // Regression test: a generic `impl<T where Bound T> Trait Source Target`
    // of a multi-parameter trait with a functional dependency used to fail
    // dispatch entirely, even at a fully concrete call site, because the
    // dependency-completion and codegen dispatch paths compared a candidate
    // impl's (possibly still-generic) header against the query with plain
    // equality instead of unification.
    let module = type_check(concat!(
        "trait Bound T { check: T -> Bool }\n",
        "trait Convert Source { type Target; convert: move Source -> Target }\n",
        "impl Bound I32 { check = value => True }\n",
        "impl <T where Bound T> Convert T { type Target = T;\n",
        "    convert = move value => value\n",
        "}\n",
        "let result: I32 = Convert.convert (I32 :: 5)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a generic impl of a trait with an associated type should dispatch and compile");
}

#[test]
fn rejects_inferred_trait_obligations_whose_impl_bounds_do_not_hold() {
    // Regression test: when a trait obligation had an inferred argument
    // position, candidate implementations were selected by unifying their
    // headers without checking their `where` bounds, so a `Copy`-gated impl
    // could be chosen for a move-only argument. Code generation then failed
    // with a confusing error located in the standard library.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.CString\n",
            "trait Make T { type U; make: T -> U }\n",
            "type Box<T> wrap (value: T)\n",
            "impl<T where Copy T> Make Box<T> { type U = T; make = Box (value) => value }\n",
            "def inferred: Box<CString> -> () = box => {\n",
            "    let value = Make.make box\n",
            "    ()\n",
            "}\n",
        )))
        .expect_err_diagnostics("an inferred trait obligation must check its impl's bounds");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("no trait implementation") })
    );
}

#[test]
fn list_supports_bracket_indexing_mutation_and_iteration() {
    let module = type_check(concat!(
        "def exercise: () -> () = () => {\n",
        "let mut values: List<I32> = List.new ()\n",
        "List.push values 10\n",
        "List.push values 20\n",
        "List.push values 30\n",
        "let index: USize = USize :: 1\n",
        "let read: I32 = values[index]\n",
        "values[index] = 99\n",
        "let mut sum: I32 = 0\n",
        "for item in values {\n",
        "  sum = sum + item\n",
        "}\n",
        "()\n}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("List Index/MutateIndex/Iterator/IntoIterator should compile");
}

#[test]
fn polymorphic_value_naming_an_associated_type_is_a_restriction() {
    // The scheme annotation checks (its projection links to the bound), but a
    // polymorphic entry cannot carry the associated type yet.
    let module = resolve(concat!(
        "use std.iterator.(Iterator, IterStep)\n",
        "def step: <Iter where Iterator Iter> move Iter -> IterStep<Iter, Iterator.Item Iter> = <Iter where Iterator Iter> move iter => Iterator.next iter\n",
        "let stored: <Iter where Iterator Iter> move Iter -> IterStep<Iter, Iterator.Item Iter> = step\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("a scheme value naming an associated type is restricted");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "polymorphic values whose type names an associated type (such as `Iterator.Item Iter`) are not supported yet"
    );
}

#[test]
fn nested_list_of_macros_check() {
    // A speculative parse of the inner expansion used to compute its error
    // location from a span in the outer file and panic on the slice.
    type_check(concat!(
        "let rows: List<List<I32>> = List.of (List.of (1, 2), List.of (3,))\n",
    ));
}

#[test]
fn list_of_macro_builds_a_list_from_values() {
    let module = type_check(concat!(
        "def exercise: () -> () = () => {\n",
        "let empty: List<I32> = List.of ()\n",
        "let one: List<I32> = List.of (42)\n",
        "let many: List<I32> = List.of (1, 2, 3, 4, 5)\n",
        "()\n}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("List.of should compile for the empty, singleton, and multi-element cases");
}

#[test]
fn wrapping_a_curried_mut_effect_call_attributes_the_right_argument() {
    // Regression test: a user-defined function wrapping a curried, 2-argument
    // `mut`-effect callee (like `Buffer.push: mut Buffer T -> T -> ()`) used
    // to require *both* of its own arguments to be `mut`, because the
    // resource/mutation inference resolved every curry depth of the call
    // chain back to the callee's full, outermost-arrow signature instead of
    // the residual type at that specific depth. Only the argument in the
    // buffer's own position should ever need to be `mut`.
    let module = type_check(concat!(
        "use std.buffer.Buffer\n",
        "def push_value: (mut Buffer<I32>, I32) -> () = (mut buffer, value) => {\n",
        "  Buffer.push buffer value\n",
        "}\n",
        "def exercise: () -> () = () => {\n",
        "let mut values: Buffer<I32> = Buffer.with_capacity (USize :: 2)\n",
        "push_value (values, 10)\n",
        "push_value (values, 20)\n",
        "let length: USize = Buffer.length values\n",
        "()\n}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("wrapped curried Buffer.push should compile");
}

#[test]
fn validates_the_standard_library_string_representation() {
    let valid_diagnostics = string_contract_diagnostics(concat!(
        "pub type String wrap SharedBuffer<U8>\n",
        "def exposed_bytes: String -> SharedBuffer<U8> = String value => value\n",
        "def matched_bytes: String -> SharedBuffer<U8> = value => match value { String bytes => bytes, }\n",
    ));
    assert!(
        valid_diagnostics.is_empty(),
        "valid String contract diagnostics: {valid_diagnostics:?}"
    );

    for (declaration, expected) in [
        (
            "pub type String opaque\n",
            "standard library type `String` must be a represented wrapper type",
        ),
        (
            "pub type String wrap pub SharedBuffer<U8>\n",
            "standard library type `String` must keep its representation private",
        ),
        (
            "pub type String<T> wrap SharedBuffer<U8>\n",
            "standard library type `String` must not accept compile-time arguments",
        ),
        (
            "pub type String wrap SharedBuffer<I8>\n",
            "standard library type `String` must be represented by `SharedBuffer<U8>`, found `SharedBuffer<I8>`",
        ),
    ] {
        let diagnostics = string_contract_diagnostics(declaration);
        assert!(
            diagnostics.iter().any(|message| message == expected),
            "missing `{expected}` in {diagnostics:?}",
        );
    }
}

#[test]
fn c_string_is_an_imported_primitive_macro() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "def exercise = () => {\n",
        "  let text: String = \"hello\"\n",
        "  let c_text: CString = c_string \"hello\"\n",
        "  let copied: String = CString.to_string_unchecked c_text\n",
        "  let converted: CString = CString.from_string_unchecked text\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .expect("String operations should compile");

    assert!(llvm.contains("string.allocate"));
    assert!(llvm.contains("string.length"));
    assert!(llvm.contains("@strlen"));
    assert!(llvm.contains("@memchr"));
    assert!(llvm.contains("@llvm.trap"));
    assert!(llvm.contains("c\"hello\\00\""));
}

#[test]
fn a_c_string_temporary_passed_to_a_borrowing_callback_is_freed() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { strlen: CString -> USize }\n",
        "def apply: (CString -> USize) -> USize = f => f (c_string \"x\")\n",
        "def apply_move: ((move CString) -> USize) -> USize = f => f (c_string \"y\")\n",
        "def borrow: CString -> USize = value => strlen value\n",
        "def consume: move CString -> USize = move value => strlen value\n",
        "let first = apply borrow\n",
        "let second = apply_move consume\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("callback calls should lower");
    let borrowed = function_definition(&llvm, "apply");
    let call = borrowed
        .find("%closure.call")
        .expect("apply calls its callback");
    assert!(
        borrowed[call..].contains("@free"),
        "the caller frees a temporary its callback only borrows:\n{borrowed}"
    );
    let moved = function_definition(&llvm, "apply_move");
    assert!(
        !moved.contains("@free"),
        "a `move` callback owns the temporary:\n{moved}"
    );
}

#[test]
fn a_block_tail_c_string_is_moved_out_instead_of_freed() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "def text: () -> CString = () => { c_string \"hello\" }\n",
        "let held = text ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a block-tail CString should lower");
    let body = function_definition(&llvm, "text");
    assert!(
        body.contains("c_string.data"),
        "the block tail should allocate the CString"
    );
    assert!(
        !body.contains("c_string.drop"),
        "a block tail must not free its CString value"
    );
}

#[test]
fn c_string_rejects_non_literal_arguments() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "use std.cinterop.*\n",
                "let text = \"hello\"\n",
                "c_string text\n",
            ),
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("c_string should require a literal during expansion");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`c_string` requires a string literal")
    );
}

#[test]
fn c_string_rejects_interior_nul_bytes() {
    let module = type_check("use std.cinterop.*\nc_string \"bad\\0value\"\n");
    let diagnostics = Lowerer::new()
        .lower(&module)
        .expect_err_diagnostics("interior NUL should fail lowering");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "C string literals cannot contain an interior NUL byte"
    }));
}

#[test]
fn expands_user_defined_macros_hygienically() {
    let module = type_check(concat!(
        "macro keep = value => parse_quote { { let temporary = 1; $value } }\n",
        "let temporary = 2\n",
        "let result: I32 = keep temporary\n",
    ));
    assert!(
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .any(|item| matches!(item, Item::MacroDeclaration(_)))
    );
}

#[test]
fn diagnostics_in_macro_generated_syntax_point_at_the_macro_definition() {
    // The `quote` body is reconstructed into a fresh token stream and re-parsed
    // when the macro expands. A diagnostic raised against that generated syntax
    // must still report a real position in the macro definition, not a
    // line/column inside the internal reconstruction.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "macro pick = (condition: Expr; body: Expr) =>\n", // line 1
            "    quote {\n",                                   // line 2
            "        match $condition {\n",                    // line 3
            "            True => $body,\n",                    // line 4
            "        }\n",                                     // line 5
            "    }\n",                                         // line 6
            "let flag: Bool = True\n",                         // line 7
            "pick flag (0)\n",                                 // line 8
        )))
        .expect_err_diagnostics("a macro expanding to a non-exhaustive match should fail");

    let non_exhaustive = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("non-exhaustive match"))
        .unwrap_or_else(|| {
            panic!(
                "expected a non-exhaustive match diagnostic, got: {:?}",
                diagnostics
                    .iter()
                    .map(|d| d.message.clone())
                    .collect::<Vec<_>>()
            )
        });

    let rendered = non_exhaustive.to_string();
    assert!(
        rendered.ends_with("at line 3, column 9"),
        "diagnostic should point at the `match` keyword in the macro body, got: {rendered}"
    );
    assert!(
        rendered.contains("<stdin>.sta"),
        "diagnostic should reference the real source file, got: {rendered}"
    );
}

#[test]
fn inspects_identifier_and_call_syntax() {
    let module = type_check(concat!(
        "macro call_argument = value: CallExpr => match value {\n",
        "    CallExpr (_, argument) => argument,\n",
        "}\n",
        "macro call_callee = value: CallExpr => value.callee\n",
        "macro classify_ident = value: Ident<String> => match value {\n",
        "    Ident \"target\" => parse_quote { 1 },\n",
        "    Ident _ => parse_quote { 2 },\n",
        "}\n",
        "def identity = (value: I32) => value\n",
        "let argument: I32 = call_argument (identity 41)\n",
        "let callee: I32 = call_callee (identity 42) 0\n",
        "let spelling: I32 = classify_ident target\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("structured syntax inspection should generate code");
}

#[test]
fn constructs_identifier_and_call_syntax() {
    let module = type_check(concat!(
        "macro generated_name = _: Expr => Ident \"answer\"\n",
        "macro generated_call = (callee: Expr; argument: Expr) =>\n",
        "    CallExpr (callee: callee, argument: argument)\n",
        "def identity = (value: I32) => value\n",
        "let answer: I32 = 40\n",
        "let name: I32 = generated_name ()\n",
        "let call: I32 = generated_call identity 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("constructed syntax should generate code");
}

#[test]
fn bare_ident_defaults_spelling_to_string() {
    let module = type_check(concat!(
        "macro classify_ident = value: Ident => match value {\n",
        "    Ident \"target\" => parse_quote { 1 },\n",
        "    Ident _ => parse_quote { 2 },\n",
        "}\n",
        "let spelling: I32 = classify_ident target\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("bare `Ident` should default its spelling to `String` and generate code");
}

#[test]
fn bare_ident_and_explicit_ident_string_are_the_same_type() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    let ident: Ident = parse_quote { name }\n",
                "    parse_quote { 0 }\n",
                "}\n",
                "let result = invalid ()\n",
            ),
            root,
        )
        .expect("unsupported contextual quote source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("narrow syntax nodes are not supported parse_quote contexts");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "Ident<String> is not a supported `parse_quote` context"
    }));
}

#[test]
fn structured_syntax_overloads_use_leaf_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Expr => parse_quote { 1 }\n",
        "macro classify = _: CallExpr => parse_quote { 2 }\n",
        "macro classify = _: UnstructuredExpr => parse_quote { 3 }\n",
        "macro classify_name = _: Expr => parse_quote { 4 }\n",
        "macro classify_name = _: Ident<String> => parse_quote { 5 }\n",
        "let call: I32 = classify (f 0)\n",
        "let other: I32 = classify 0\n",
        "let name: I32 = classify_name identifier\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("structured syntax overloads should select a unique leaf");
}

#[test]
fn constructed_identifiers_use_definition_hygiene_and_children_keep_caller_hygiene() {
    let module = type_check(concat!(
        "macro definition_name = _: Expr => Ident \"captured\"\n",
        "macro apply = (callee: Expr; argument: Expr) =>\n",
        "    CallExpr (callee: callee, argument: argument)\n",
        "let captured: I32 = 7\n",
        "def definition_site = (captured: String) => definition_name ()\n",
        "def caller_site = (local: (I32 -> I32)) => apply local 8\n",
        "let first: I32 = definition_site \"shadow\"\n",
        "let second: I32 = caller_site (value => value)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("constructed syntax should retain the appropriate hygiene context");
}

#[test]
fn mutates_call_syntax_with_value_semantics_and_shared_capture_cells() {
    let module = type_check(concat!(
        "macro replace_call = (value: CallExpr; replacement: Expr) => {\n",
        "    let original = value\n",
        "    let mut changed = value\n",
        "    changed.argument = replacement\n",
        "    parse_quote { ($original, $changed) }\n",
        "}\n",
        "macro replace_from_closure = (value: CallExpr; replacement: Expr) => {\n",
        "    let mut changed = value\n",
        "    let update = () => { changed.argument = replacement; () }\n",
        "    update ()\n",
        "    changed\n",
        "}\n",
        "def identity = (value: I32) => value\n",
        "let pair: (I32, I32) = replace_call (identity 1) 2\n",
        "let captured: I32 = replace_from_closure (identity 3) 4\n",
    ));
    let pair = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| {
            let item = item;
            let Item::Binding(binding) = item else {
                return None;
            };
            (binding.name == "pair").then_some(binding.value.as_ref()?)
        })
        .expect("expanded pair binding");
    let staple_syntax::Expression::Product(pair) = pair else {
        panic!("replacement macro should expand to a product");
    };
    let arguments = pair
        .elements
        .iter()
        .map(|element| {
            let staple_syntax::Expression::Call(call) = &element.value else {
                panic!("pair element should remain a call");
            };
            let staple_syntax::Expression::Integer(argument) = call.argument.as_ref() else {
                panic!("call argument should be an integer");
            };
            argument.literal.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(arguments, ["1", "2"]);

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("mutated syntax should generate code");
}

#[test]
fn diagnoses_invalid_structured_syntax_operations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "macro invalid = value: CallExpr => { value.argument = parse_quote { 1 }; value }\ninvalid (f 0)\n",
            "cannot write through immutable compile-time binding `value`",
        ),
        (
            "macro invalid = value: CallExpr => value.missing\ninvalid (f 0)\n",
            "call syntax has no field `missing`",
        ),
        (
            "macro invalid = value: Expr => CallExpr (callee: value, argument: 1)\ninvalid f\n",
            "`CallExpr.argument` must contain `Expr`",
        ),
        (
            "macro invalid = value: Expr => Ident \"not an identifier\"\ninvalid f\n",
            "`not an identifier` is not a valid identifier spelling",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid structured syntax operation should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn rejects_structured_syntax_values_at_runtime() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "let generated: CallExpr = 1\n",
            "`Syntax` values are compile-time-only",
        ),
        (
            "let generated = Ident \"name\"\n",
            "`Ident` syntax values are compile-time-only",
        ),
        (
            "let generated = CallExpr (callee: 1, argument: 2)\n",
            "`CallExpr` syntax values are compile-time-only",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root);
        let program = match program {
            Ok(program) => program,
            Err(error)
                if expected == "`@doc` may only modify a named declaration"
                    && error.contains("documentation comments require a named declaration") =>
            {
                continue;
            }
            Err(error) => panic!("source should parse: {error}"),
        };
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("runtime syntax values should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn expands_explicit_and_inferred_item_macros() {
    let module = type_check(concat!(
        "macro define_answer: Expr -> Item = value => parse_quote {\n",
        "    def answer = () => $value\n",
        "}\n",
        "def item_identity: Item -> Item = item => item\n",
        "macro define_type = _: Expr => item_identity parse_quote { type Generated }\n",
        "define_answer 42\n",
        "define_type ()\n",
        "let result: I32 = answer ()\n",
        "let generated: Generated = Generated\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, item
            if matches!(item, Item::Binding(binding)
                if binding.name == "answer"))
    }));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::TypeDeclaration(declaration) if declaration.name == "Generated")
    }));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generated bindings and types should generate code");
}

#[test]
fn expands_item_modifier_macros_with_nearest_modifier_first() {
    let module = type_check(concat!(
        "macro @identity = item => item\n",
        "macro @outer: Item -> Item = _ => parse_quote { let selected: I32 = 42 }\n",
        "macro @inner: Item -> Item = _ => parse_quote { let selected: String = \"inner\" }\n",
        "@identity\n",
        "@outer\n",
        "@inner\n",
        "let original = 0\n",
        "let result: I32 = selected\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, item
            if matches!(item, Item::Binding(binding)
                if binding.name == "selected"))
    }));
}

#[test]
fn modifier_arguments_support_expression_type_and_pattern_syntax() {
    let module = type_check(concat!(
        "macro @value = (value; _) => parse_quote { let generated: I32 = $value }\n",
        "macro @typed: (Parenthesized<Type>; Item) -> Item = (ty; _) => parse_quote { let typed: $ty = 1 }\n",
        "macro @bind: (Parenthesized<Pattern>; Item) -> Item = (pattern; _) => parse_quote { let $pattern = (40, 2) }\n",
        "@value(42)\n",
        "let replaced = 0\n",
        "@typed(I32)\n",
        "let typed_original = 0\n",
        "@bind((left, right))\n",
        "let destructured = 0\n",
        "let result: I32 = typed + left + right\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("modifier arguments and generated items should compile");
}

#[test]
fn expands_modifier_lists_generated_by_item_macros() {
    let module = type_check(concat!(
        "macro @replace: Item -> Item = _ => parse_quote { let generated: I32 = 42 }\n",
        "macro emit: Expr -> Item = _: Expr => parse_quote { @replace let original = 0 }\n",
        "emit ()\n",
        "let result: I32 = generated\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generated modifier lists should expand before resolution");
}

#[test]
fn builtin_doc_composes_with_modifier_expansion_in_source_order() {
    let module = type_check(concat!(
        "macro @identity: Item -> Item = item => item\n",
        "@doc(\"outer\")\n",
        "@identity\n",
        "///inner\n",
        "let documented: I32 = 42\n",
    ));
    let binding = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == "documented" => Some(binding),
            _ => None,
        })
        .expect("documented binding should survive expansion");
    assert_eq!(binding.docs, ["outer", "inner"]);
}

#[test]
fn modifier_macro_produces_multiple_items_as_the_outermost_modifier() {
    let module = type_check(concat!(
        "macro @split: Item -> Sequence<Item> = _ => parse_quote {\n",
        "    let first: I32 = 1\n",
        "    let second: I32 = 2\n",
        "}\n",
        "@split\n",
        "let original: I32 = 0\n",
        "let result: I32 = first + second\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("modifier producing a Sequence Item result should compile");
}

#[test]
fn modifier_macro_produces_multiple_items_via_raw_syntax() {
    let module = type_check(concat!(
        "macro @spread: Item -> Syntax = _ => quote {\n",
        "    let a: I32 = 1\n",
        "    let b: I32 = 2\n",
        "}\n",
        "@spread\n",
        "let original: I32 = 0\n",
        "let result: I32 = a + b\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("modifier producing raw Syntax that reparses to multiple items should compile");
}

#[test]
fn modifier_macro_deletes_its_target_by_producing_zero_items() {
    let module = type_check(concat!(
        "macro @delete: Item -> Sequence<Item> = _ => parse_quote {}\n",
        "@delete\n",
        "let removed: I32 = 0\n",
        "let result: I32 = 1\n",
    ));
    assert!(!module.resolved().syntax().items.iter().any(|item| {
        matches!(item, item
            if matches!(item, Item::Binding(binding)
                if binding.name == "removed"))
    }));
}

#[test]
fn block_modifiers_replace_splice_and_delete_items() {
    let module = type_check(concat!(
        "macro @split: Item -> Sequence<Item> = _ => parse_quote {\n",
        "    let first: I32 = 20\n",
        "    let second: I32 = 22\n",
        "}\n",
        "macro @delete: Item -> Sequence<Item> = _ => parse_quote {}\n",
        "def answer: () -> I32 = () => {\n",
        "    @split let original = 0\n",
        "    @delete let removed = 0\n",
        "    first + second\n",
        "}\n",
        "let result: I32 = answer ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("block modifier output should splice and compile");
}

#[test]
fn block_modifiers_may_generate_supported_declarations() {
    let module = type_check(concat!(
        "mod helpers { pub def value: I32 = 42 }\n",
        "macro @declarations: Item -> Sequence<Item> = _ => parse_quote {\n",
        "    type Local = I32\n",
        "    use helpers.value\n",
        "    mod local { pub def extra: I32 = 1 }\n",
        "    let generated: Local = value + local.extra\n",
        "}\n",
        "def answer: () -> I32 = () => {\n",
        "    @declarations let original = 0\n",
        "    generated\n",
        "}\n",
        "let result: I32 = answer ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("supported declarations generated in a block should compile");
}

#[test]
fn block_modifiers_reject_unsupported_and_public_outputs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        concat!(
            "macro @invalid: Item -> Item = _ => parse_quote { trait Invalid T { value: T } }\n",
            "def test = () => { @invalid let original = 0 }\n",
        ),
        concat!(
            "macro @invalid: Item -> Item = _ => parse_quote { pub let invalid = 0 }\n",
            "def test = () => { @invalid let original = 0 }\n",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid block modifier output should be rejected");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message == "item is not supported in a block expression"
            }),
            "expected unsupported block item diagnostic, found {diagnostics:#?}",
        );
    }
}

#[test]
fn outermost_modifier_can_discard_remaining_modifiers() {
    let module = type_check(concat!(
        "macro @outer: Item -> Item = _ => parse_quote { let transformed: I32 = 100 }\n",
        "macro @inner: Item -> Sequence<Item> = _ => parse_quote {\n",
        "    let first: I32 = 1\n",
        "    let second: I32 = 2\n",
        "}\n",
        "@outer\n",
        "@inner\n",
        "let original: I32 = 0\n",
        "let result: I32 = transformed\n",
    ));
    assert!(!module.resolved().syntax().items.iter().any(|item| {
        matches!(item, item
            if matches!(item, Item::Binding(binding)
                if binding.name == "first"))
    }));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("the discarded inner modifier should not run");
}

#[test]
fn outermost_multi_item_result_discards_remaining_modifiers() {
    let module = type_check(concat!(
        "macro @outer: Item -> Sequence<Item> = _ => parse_quote {\n",
        "    let replaced_first: I32 = 100\n",
        "    let from_outer: I32 = 200\n",
        "}\n",
        "macro @inner: Item -> Sequence<Item> = _ => parse_quote {\n",
        "    let first: I32 = 1\n",
        "    let second: I32 = 2\n",
        "}\n",
        "@outer\n",
        "@inner\n",
        "let original: I32 = 0\n",
        "let result: I32 = replaced_first + from_outer\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("the outer modifier's items should replace the entire modified item");
}

#[test]
fn returning_modified_input_continues_to_a_zero_item_modifier() {
    let source = concat!(
        "macro @outer: Item -> Item = item => item\n",
        "macro @inner: Item -> Sequence<Item> = _ => parse_quote {}\n",
        "@outer\n",
        "@inner\n",
        "let original: I32 = 0\n",
    );
    let module = type_check(source);
    assert!(
        !module
            .resolved()
            .syntax()
            .items
            .iter()
            .any(|item| { matches!(item, Item::Binding(binding) if binding.name == "original") })
    );
}

#[test]
fn modified_item_can_be_destructured_and_reconstructed_losslessly() {
    let module = type_check(concat!(
        "use std.syntax.(ModifiedItem, parse_quote, Item)\n",
        "macro @outer: Item -> Item = item => match item {\n",
        "    ModifiedItem (modifiers, inner) => ModifiedItem (modifiers: modifiers, item: inner),\n",
        "    _ => panic \"outer item is unsupported\",\n",
        "}\n",
        "macro @inner: (Parenthesized<Expr>; Item) -> Item = (value; _) => parse_quote { let reconstructed: I32 = $value }\n",
        "@outer\n",
        "@inner(1)\n",
        "let original: I32 = 0\n",
        "let result: I32 = reconstructed\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("reconstructing `ModifiedItem` should preserve and continue its modifier");
}

#[test]
fn expands_metadata_aware_macros_and_contextual_visibility_splices() {
    let module = type_check(concat!(
        "def normalize_visibility: Visibility -> Visibility = value => value\n",
        "def visibility_number: Visibility -> Expr = value => match value {\n",
        "    Private => parse_quote { 1 },\n",
        "    Package => parse_quote { 2 },\n",
        "    Public => parse_quote { 3 },\n",
        "}\n",
        "macro define_alias = (metadata: MacroCallMetadata; ty: Type) => {\n",
        "    let actual = normalize_visibility metadata.visibility\n",
        "    parse_quote { $actual type Generated = $ty }\n",
        "}\n",
        "macro classify = (before: Expr; vis: Visibility; after: Expr) => {\n",
        "    let number = visibility_number vis\n",
        "    number\n",
        "}\n",
        "macro call_visibility = metadata: MacroCallMetadata => visibility_number metadata.visibility\n",
        "macro first_visibility = (vis: Visibility; value: Expr) => visibility_number vis\n",
        "macro final_visibility = (value: Expr; vis: Visibility) => visibility_number vis\n",
        "pub define_alias I32\n",
        "let implicit: I32 = classify 10 20\n",
        "let public: I32 = classify 10 pub 20\n",
        "let package_vis: I32 = classify 10 pub(package) 20\n",
        "let private_call: I32 = call_visibility\n",
        "let first_private: I32 = first_visibility 0\n",
        "let first_public: I32 = first_visibility pub 0\n",
        "let final_private: I32 = final_visibility 0\n",
        "let final_public: I32 = final_visibility 0 pub\n",
        "let generated: Generated = 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("metadata-aware macro output should compile");
}

#[test]
fn expands_standard_typegroup_into_module_variants_and_alias() {
    let module = type_check(concat!(
        "pub typegroup Pattern {\n",
        "    Literal wrap pub String,\n",
        "    Wildcard,\n",
        "}\n",
        "pub use Pattern.*\n",
        "let literal: Pattern = Pattern.Literal \"value\"\n",
        "let wildcard: Pattern = Pattern.Wildcard\n",
        "let reexported_literal: Pattern = Literal \"value\"\n",
        "let reexported_wildcard: Pattern = Wildcard\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generated typegroup variants and alias should compile");
}

#[test]
fn typegroup_applies_call_modifiers_to_the_alias_and_entry_modifiers_to_variants() {
    let resolved = resolve(concat!(
        "@doc(\"alias docs\")\n",
        "typegroup Tagged T {\n",
        "    ///outer\n",
        "    @doc(\"inner\")\n",
        "    Marked wrap T,\n",
        "    Plain,\n",
        "}\n",
    ));
    let alias = resolved
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::TypeDeclaration(declaration) if declaration.name == "Tagged" => Some(declaration),
            _ => None,
        })
        .expect("typegroup should generate its alias");
    assert_eq!(alias.docs, ["alias docs"]);

    let companion = resolved
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::Submodule(module) if module.companion && module.name == "Tagged" => Some(module),
            _ => None,
        })
        .expect("typegroup should generate its companion");
    assert!(
        companion.docs.is_empty(),
        "call modifiers must not leak to the companion"
    );
    let variants = companion
        .module
        .items
        .iter()
        .filter_map(|item| match item {
            Item::TypeDeclaration(declaration) => Some(declaration),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0].name, "Marked");
    // Docs keep source order: the `///outer` line precedes `@doc("inner")`.
    assert_eq!(variants[0].docs, ["outer", "inner"]);
    assert_eq!(variants[1].name, "Plain");
    assert!(variants[1].docs.is_empty());
}

#[test]
fn typegroup_alias_keeps_multi_line_doc_comment_order() {
    // The `typegroup` call's `///` lines reach the generated alias as a run of
    // `@doc` modifiers. Attaching them must preserve reading order rather than
    // reversing it.
    let resolved = resolve(concat!(
        "/// The result of comparing two ordered values.\n",
        "///\n",
        "/// Refer to the variants as `Ordering.Less`, `Ordering.Equal`, and\n",
        "/// `Ordering.Greater`.\n",
        "pub typegroup Ordering {\n",
        "    Less,\n",
        "    Equal,\n",
        "    Greater,\n",
        "}\n",
    ));
    let alias = resolved
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::TypeDeclaration(declaration) if declaration.name == "Ordering" => {
                Some(declaration)
            }
            _ => None,
        })
        .expect("typegroup should generate its alias");
    assert_eq!(
        alias.docs,
        [
            " The result of comparing two ordered values.",
            "",
            " Refer to the variants as `Ordering.Less`, `Ordering.Equal`, and",
            " `Ordering.Greater`.",
        ]
    );
}

#[test]
fn resolves_and_merges_type_companion_items() {
    type_check(concat!(
        "type Animal from I32\n",
        "let offset: I32 = 1\n",
        "companion Animal { pub def move_to = animal: Animal => animal.* + offset }\n",
        "companion Animal { pub def stop = animal: Animal => animal }\n",
        "let moved: Animal = Animal.move_to 1\n",
        "let stopped: Animal = Animal.stop moved\n",
    ));
    type_check(concat!(
        "type Box<T> from (value: T)\n",
        "companion<T> Box<T> { pub def box_identity: move Box<T> -> Box<T> = move box => box }\n",
        "let identity: Box<I32> -> Box<I32> = Box.box_identity\n",
    ));
}

#[test]
fn alias_companions_extend_the_aliased_type() {
    // Aliases are transparent: `companion Pet` is `companion Animal`, so its
    // members are reachable through either name and by method syntax.
    type_check(concat!(
        "type Animal wrap I32\n",
        "type Pet = Animal\n",
        "companion Pet { pub def legs: Animal -> I32 = animal => 4 }\n",
        "companion Animal { pub def name: Animal -> String = animal => \"animal\" }\n",
        "let animal = Animal 1\n",
        "let from_alias: I32 = Animal.legs animal\n",
        "let from_type: String = Pet.name animal\n",
        "let method: I32 = animal |> legs\n",
    ));
}

#[test]
fn rejects_alias_companions_of_another_package_s_types() {
    // An alias is transparent, so its companion belongs to the aliased type,
    // which here the standard library declares: the structural `I32 | String`
    // and `(T, T)` have no component of this package, and `I32` is
    // nominal.
    let diagnostics = resolve_result(concat!(
        "type Either = I32 | String\n",
        "companion Either { pub def zero = 0 }\n",
        "type Pair<T> = (T, T)\n",
        "companion<T> Pair<T> {}\n",
        "type Number = I32\n",
        "companion Number { pub def one = 1 }\n",
    ))
    .expect_err_diagnostics("alias companions follow the package orphan rule");
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            "companion target `Either` has no component declared in the current package; a structural companion needs a type of its own package in its target",
            "companion target `Pair<T>` has no component declared in the current package; a structural companion needs a type of its own package in its target",
            "companion target `Number` is declared in another package; a package can add companion items only to its own types",
        ]
    );
}

#[test]
fn typegroup_companion_merges_with_a_hand_written_companion_block() {
    // A generated `companion` from `typegroup` must merge with an explicit
    // `companion` block for the same type instead of registering a second
    // module of the same name (which surfaced as `duplicate import`).
    type_check(concat!(
        "typegroup Switch {\n",
        "    Enabled,\n",
        "    Disabled,\n",
        "}\n",
        "companion Switch { pub def flip = switch: Switch => switch }\n",
        "use Switch.*\n",
        "let on: Switch = Enabled\n",
        "let flipped: Switch = Switch.flip on\n",
    ));
    // Order-independent: explicit block before the `typegroup`.
    type_check(concat!(
        "companion Toggle { pub def flip = toggle: Toggle => toggle }\n",
        "typegroup Toggle {\n",
        "    On,\n",
        "    Off,\n",
        "}\n",
        "use Toggle.*\n",
        "let lit: Toggle = On\n",
        "let flipped: Toggle = Toggle.flip lit\n",
    ));
}

#[test]
fn type_checks_companion_method_call_syntax() {
    let module = type_check(concat!(
        "type Animal from I32\n",
        "companion Animal { pub def move_to: Animal -> (F32, F32) -> Animal = animal => _ => animal }\n",
        "let animal: Animal = 1\n",
        "let moved: Animal = animal |> move_to (1.0, 1.0)\n",
        "let partially_applied: (F32, F32) -> Animal = animal |> move_to\n",
        "def relocate: Animal -> Animal = value => value |> move_to (1.0, 1.0)\n",
        "def make: () -> Animal = () => animal\n",
        "let moved_from_call: Animal = (make ()) |> move_to (1.0, 1.0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("companion method calls should lower as ordinary calls");
}

#[test]
fn type_checks_method_call_syntax_for_juxtaposed_companion_methods() {
    let module = type_check(concat!(
        "type Animal from I32\n",
        "companion Animal {\n",
        "    pub def move_to: (Animal; (F32, F32)) -> Animal = (animal; _) => animal\n",
        "    pub def teleport: (Animal; F32; F32) -> Animal = (animal; _; _) => animal\n",
        "}\n",
        "let animal: Animal = 1\n",
        "let moved: Animal = animal |> move_to (1.0, 1.0)\n",
        "let hopped: Animal = animal |> teleport 1.0 2.0\n",
        "def relocate: Animal -> Animal = value => value |> move_to (1.0, 1.0)\n",
        "def make: () -> Animal = () => animal\n",
        "let moved_from_call: Animal = (make ()) |> teleport 1.0 2.0\n",
        "companion Animal { pub def tag: <T> (Animal; T) -> Animal = <T> (animal; _) => animal }\n",
        "let tagged: Animal = animal |> tag 7\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("juxtaposed companion method calls should lower as ordinary juxtaposed calls");
}

#[test]
fn pipes_into_local_functions_parameters_and_closures() {
    let module = type_check(concat!(
        "def double = value: I32 => value * 2\n",
        "def add: (I32; I32) -> I32 = (left; right) => left + right\n",
        "let triple = value: I32 => value * 3\n",
        "let offset = 10\n",
        "let doubled: I32 = 21 |> double\n",
        "let added: I32 = 1 |> add 2\n",
        "let chained: I32 = 3 |> double |> add 1 |> triple\n",
        "def apply_twice = (f: I32 -> I32; value: I32) => value |> f |> f\n",
        "def shifted = () => {\n",
        "    let shift = value: I32 => value + offset\n",
        "    let later = () => 1 |> shift\n",
        "    later ()\n",
        "}\n",
        "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = <T, effect E> callback => callback ()\n",
        "def thunked = () => {\n",
        "    let shift = value: I32 => value + offset\n",
        "    evaluate { 1 |> shift }\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("pipes into local functions should lower as ordinary calls");
}

#[test]
fn rejects_pipe_stages_matching_a_local_and_a_companion_or_trait() {
    let messages = type_check_errors(
        concat!(
            "type Animal wrap I32\n",
            "companion Animal { pub def legs: Animal -> I32 = animal => 4 }\n",
            "trait Named T { name: T -> String }\n",
            "impl Named I32 { name = value => \"trait\" }\n",
            "def companion_clash = (legs: Animal -> I32) => Animal 1 |> legs\n",
            "def trait_clash = (name: I32 -> String) => 1 |> name\n",
        ),
        "a local and a companion or trait stage of the same name are ambiguous",
    );
    for expected in [
        "ambiguous pipe stage `legs`: both the local `legs` and the companion function `Animal.legs`",
        "ambiguous pipe stage `name`: both the local `name` and the trait method `Named.name`",
    ] {
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "missing `{expected}` in {messages:?}"
        );
    }
}

#[test]
fn qualified_or_incompatible_locals_leave_pipe_stages_unambiguous() {
    type_check(concat!(
        "type Animal wrap I32\n",
        "companion Animal { pub def legs: Animal -> I32 = animal => 4 }\n",
        "trait Named T { name: T -> String }\n",
        "impl Named I32 { name = value => \"trait\" }\n",
        "def qualified = (legs: Animal -> I32) => Animal 1 |> Animal.legs\n",
        "def direct = (legs: Animal -> I32) => legs (Animal 1)\n",
        "def qualified_trait = (name: I32 -> String) => 1 |> Named.name\n",
        "def not_callable = (legs: I32) => Animal 1 |> legs\n",
        "def incompatible = (legs: String -> I32) => Animal 1 |> legs\n",
        "def trait_wins = (name: String -> String) => 1 |> name\n",
    ));
}

#[test]
fn rejects_incomplete_method_call_on_a_juxtaposed_companion_method() {
    TypeChecker::new()
        .check(resolve(concat!(
            "type Animal from I32\n",
            "companion Animal {\n",
            "    pub def move_to: (Animal; (F32, F32)) -> Animal = (animal; _) => animal\n",
            "}\n",
            "let animal: Animal = 1\n",
            "let incomplete = animal |> move_to\n",
        )))
        .expect_err_diagnostics(
            "a receiver-applied juxtaposed companion method is an incomplete call",
        );
}

#[test]
fn typegroup_supports_generic_groups_and_reexports_their_variants() {
    let module = type_check(concat!(
        "pub typegroup Maybe T {\n",
        "    Missing,\n",
        "    Present wrap pub T,\n",
        "}\n",
        "pub use Maybe.*\n",
        "let missing: Maybe<I32> = Missing\n",
        "let present: Maybe<I32> = Present 1\n",
        "let qualified: Maybe<String> = Maybe.Present \"value\"\n",
        "pub typegroup Either L R {\n",
        "    Left wrap pub L,\n",
        "    Right wrap pub R,\n",
        "}\n",
        "pub typegroup Mixed A B C D {\n",
        "    Empty,\n",
        "    Value wrap pub (A, B, C, D),\n",
        "}\n",
        "let value: (I32, String, Bool, F64) = (1, \"two\", True, 4.0)\n",
        "let mixed: Mixed<I32, String, Bool, F64> = Mixed.Value value\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generic typegroup variants and their reexports should compile");
}

#[test]
fn typegroup_alias_variants_participate_in_the_generated_sum() {
    let module = type_check(concat!(
        "pub typegroup Example {\n",
        "    Wrapped wrap pub I32,\n",
        "    Number = I32,\n",
        "    Empty,\n",
        "}\n",
        "pub use Example.*\n",
        "let wrapped: Example = Wrapped 1\n",
        "let empty: Example = Empty\n",
        "let number: Example.Number = 2\n",
    ));
    let alias = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::TypeDeclaration(declaration) if declaration.name == "Example" => {
                Some(declaration)
            }
            _ => None,
        })
        .expect("typegroup should generate its alias");
    assert!(
        alias
            .underlying()
            .is_some_and(|ty| matches!(ty, Type::Sum(_)))
    );
}

#[test]
fn typegroup_rejects_opaque_variants() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("typegroup Hidden { Secret opaque, }\n"),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("opaque typegroup variants should be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("opaque typegroup variants are unsupported")),
        "expected an opaque-variant diagnostic, found {diagnostics:#?}",
    );
}

#[test]
fn equals_and_fat_arrow_are_structured_syntax_nodes() {
    let module = type_check(concat!(
        "macro punctuation = (_: Ident<String>; equal: Equals; _: Ident<String>; arrow: FatArrow; _: Braced<Sequence<SyntaxNode>>) =>\n",
        "    match (equal, arrow, Equals, FatArrow) {\n",
        "        (Equals, FatArrow, Equals, FatArrow) => parse_quote { let punctuated: I32 = 42 },\n",
        "    }\n",
        "punctuation marker = T => {}\n",
        "let result: I32 = punctuated\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("equals and fat-arrow syntax should match and construct");
}

#[test]
fn parse_quote_produces_punctuation_result_types() {
    let module = type_check(concat!(
        "macro punctuated: Expr -> Expr = _: Expr => {\n",
        "    let comma: Comma = parse_quote { , }\n",
        "    let equals: Equals = parse_quote { = }\n",
        "    let arrow: FatArrow = parse_quote { => }\n",
        "    match (comma, equals, arrow) {\n",
        "        (Comma, Equals, FatArrow) => parse_quote { 42 },\n",
        "    }\n",
        "}\n",
        "let result: I32 = punctuated (0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("comma, equals, and fat-arrow parse_quote results should construct");
}

#[test]
fn parse_quote_produces_visibility_result_type() {
    let module = type_check(concat!(
        "macro visibility: Expr -> Expr = _: Expr => {\n",
        "    let none: Visibility = parse_quote { }\n",
        "    let package_vis: Visibility = parse_quote { pub(package) }\n",
        "    let pub_: Visibility = parse_quote { pub }\n",
        "    match (none, package_vis, pub_) {\n",
        "        (Private, Package, Public) => parse_quote { 42 },\n",
        "        _ => panic \"unexpected visibility values are unsupported\",\n",
        "    }\n",
        "}\n",
        "let result: I32 = visibility (0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("private, package, and public parse_quote results should construct");
}

#[test]
fn parse_quote_produces_delimited_result_types() {
    let module = type_check(concat!(
        "macro delimited: Expr -> Expr = _: Expr => {\n",
        "    let fixed: Parenthesized<(Ident<String>; Comma; Ident<String>)> = parse_quote { (a, b) }\n",
        "    let sequence: Bracketed<Sequence<Ident<String>>> = parse_quote { [one two] }\n",
        "    let separated: Braced<Separated<Ident<String>, Comma>> = parse_quote { { x, y, } }\n",
        "    let nested: Parenthesized<Syntax> = parse_quote { ((a), (b)) }\n",
        "    match (fixed, sequence, separated) {\n",
        "        (Parenthesized (Ident \"a\"; Comma; Ident \"b\"),\n",
        "         Bracketed (Sequence (Ident \"one\", Ident \"two\")),\n",
        "         Braced (Separated (separator: Comma, elements: (Ident<\"x\">, _), trailing: True<()>))) =>\n",
        "            match nested { Parenthesized _ => parse_quote { 42 } },\n",
        "        _ => panic \"unexpected delimited syntax is unsupported\",\n",
        "    }\n",
        "}\n",
        "let result: I32 = delimited (0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect(
            "fixed, sequence, and separated parse_quote results should construct, including a \
             nested same-kind delimiter captured opaquely as Syntax",
        );
}

#[test]
fn parse_quote_rejects_spliced_delimited_result() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro bad: Expr -> Expr = value: Expr => {\n",
                "    let x: Parenthesized<(Expr, Expr)> = parse_quote { ($value, $value) }\n",
                "    parse_quote { 0 }\n",
                "}\n",
                "let result: I32 = bad (1)\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("splicing into a delimited-result quotation should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .starts_with("quotation cannot be interpreted as Parenthesized")
    }));
}

#[test]
fn parse_quote_rejects_spliced_visibility_result() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro bad = vis: Visibility => {\n",
                "    let x: Visibility = parse_quote { $vis }\n",
                "    parse_quote { 0 }\n",
                "}\n",
                "let result = bad pub\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("splicing a whole Visibility value should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("contains visibility syntax, not expression syntax")
    }));
}

#[test]
fn rejects_legacy_typegroup_call_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        "typegroup Legacy = { Unit, }\n",
        "typegroup Legacy = T => { Wrapped T, }\n",
        "typegroup Legacy = (T, E) => { Left T, Right E, }\n",
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("legacy syntax should still parse as an ordinary call");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("legacy typegroup syntax should not expand");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("macro `typegroup` requires")
                    || diagnostic
                        .message
                        .contains("no overload of macro `typegroup`")
            }),
            "expected a typegroup overload diagnostic, found {diagnostics:#?}",
        );
    }
}

#[test]
fn typegroup_variants_require_an_explicit_reexport() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "pub typegroup Status { Ready, }\n",
                "let qualified: Status = Status.Ready\n",
                "let unqualified: Status = Ready\n",
            ),
            root,
        )
        .expect("typegroup source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("unqualified variants should require an explicit use");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown name `Ready`")),
        "expected an unknown variant diagnostic, found {diagnostics:#?}",
    );
}

#[test]
fn typegroup_supports_private_construction_compound_types_and_trailing_commas() {
    let module = type_check(concat!(
        "typegroup Local {\n",
        "    Pair wrap pub (I32, String),\n",
        "    Empty,\n",
        "}\n",
        "pub typegroup Generic {\n",
        "    Wrapped wrap pub Option<I32>,\n",
        "}\n",
        "pub typegroup PublicGroup {\n",
        "    Unit,\n",
        "}\n",
        "let pair: Local = Local.Pair (1, \"value\")\n",
        "let empty: Local = Local.Empty\n",
        "let wrapped: Generic = Generic.Wrapped (Some 1)\n",
        "let unit: PublicGroup = PublicGroup.Unit\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("private and compound typegroups should compile");
}

#[test]
fn rejects_empty_typegroups_and_top_level_optional_macro_parameters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "typegroup Empty {}\n",
            "macro `typegroup` panicked: empty typegroup is unsupported",
        ),
        (
            "macro invalid: Option<Type> -> Expr = value => parse_quote { 0 }\ninvalid I32\n",
            "`Option` and slotted product syntax shapes may only appear inside delimited contents",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid typegroup infrastructure use should fail");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn macro_call_metadata_can_generate_a_public_representation() {
    let module = type_check(concat!(
        "macro define_box = metadata: MacroCallMetadata => { let visibility = metadata.visibility; parse_quote { $visibility type Box wrap pub I32 } }\n",
        "pub define_box\n",
        "let boxed: Box = Box 42\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::TypeDeclaration(declaration)
            if declaration.name == "Box"
                && declaration.visibility == staple_syntax::Visibility::Public
                && declaration.representation_visibility() == staple_syntax::Visibility::Public)
    }));
}

#[test]
fn modifiers_compose_after_metadata_aware_item_calls() {
    let module = type_check(concat!(
        "macro @identity: Item -> Item = item => item\n",
        "macro define = metadata: MacroCallMetadata => { let visibility = metadata.visibility; parse_quote { $visibility let generated: I32 = 42 } }\n",
        "@identity\n",
        "pub define\n",
        "let result: I32 = generated\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, item
            if matches!(item, Item::Binding(binding)
                if binding.name == "generated"
                    && binding.visibility == staple_syntax::Visibility::Public))
    }));
}

#[test]
fn modifiers_before_metadata_calls_are_owned_by_the_macro() {
    let module = type_check(concat!(
        "macro @replace: Item -> Item = _ => parse_quote { let replaced: I32 = 0 }\n",
        "macro generate: MacroCallMetadata -> Item = (modifiers, visibility) => match modifiers {\n",
        "    Sequence () => parse_quote { let no_modifier: I32 = 0 },\n",
        "    _ => parse_quote { let generated: I32 = 42 },\n",
        "}\n",
        "@replace generate\n",
        "let result: I32 = generated\n",
    ));
    assert!(
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .any(|item| { matches!(item, Item::Binding(binding) if binding.name == "generated") })
    );
    assert!(
        !module
            .resolved()
            .syntax()
            .items
            .iter()
            .any(|item| { matches!(item, Item::Binding(binding) if binding.name == "replaced") })
    );
}

#[test]
fn metadata_aware_calls_can_return_expression_syntax() {
    let module = type_check(concat!(
        "macro expression: MacroCallMetadata -> Expr = metadata => parse_quote { 1 }\n",
        "@ignored expression\n",
    ));
    assert!(
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .any(|item| matches!(item, Item::Expression(_)))
    );
}

#[test]
fn source_location_macros_expand_to_the_call_site() {
    let module = type_check(concat!(
        "use std.source.(line, column, file)\n",
        "macro here = () => parse_quote { (line (), column ()) }\n",
        "macro same = (value: Expr) => parse_quote { $value }\n",
        "let direct = line ()\n",
        "let indented =    column ()\n",
        "let path = file ()\n",
        "let nested =\n",
        "    here ()\n",
        "let spliced = same (\n",
        "  line ())\n",
    ));
    let value = |name: &str| {
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("binding `{name}` should exist"))
    };
    let integer = |expression: &staple_syntax::Expression| match expression {
        staple_syntax::Expression::Integer(integer) => integer.literal.clone(),
        other => panic!("expected an integer literal, found {other:?}"),
    };
    let elements = |expression: staple_syntax::Expression| match expression {
        staple_syntax::Expression::Product(product) => product
            .elements
            .iter()
            .map(|element| element.value.clone())
            .collect::<Vec<_>>(),
        other => panic!("expected a product, found {other:?}"),
    };

    assert_eq!(integer(&value("direct")), "4");
    assert_eq!(integer(&value("indented")), "19");
    match value("path") {
        staple_syntax::Expression::String(string) => assert_eq!(string.literal, "\"<stdin>.sta\""),
        other => panic!("expected a string literal, found {other:?}"),
    }
    // Produced by a macro template: the outermost invocation's location.
    let nested = elements(value("nested"));
    assert_eq!(integer(&nested[0]), "8");
    assert_eq!(integer(&nested[1]), "5");
    // Written at the invocation site and spliced: its own location.
    assert_eq!(integer(&value("spliced")), "10");
}

#[test]
fn source_location_macros_are_not_reexported() {
    for name in ["line", "column", "file"] {
        let diagnostics = resolve_result(&format!("let value = {name} ()\n"))
            .expect_err_diagnostics("source location macros require an explicit import");
        assert!(!diagnostics.is_empty(), "`{name}` should be unbound");
    }
}

#[test]
fn diagnoses_invalid_metadata_macro_uses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "macro invalid: (Expr; MacroCallMetadata) -> Expr = (left; metadata) => left\n",
            "macro `invalid` may use `MacroCallMetadata` only as its first parameter",
        ),
        (
            "def invalid: MacroCallMetadata -> Visibility = value => value\n",
            "`MacroCallMetadata` may only be the first parameter of a function-style macro",
        ),
        (
            "macro ordinary: Expr -> Expr = value => parse_quote { $value }\npub ordinary 1\n",
            "macro `ordinary` has no overload whose first parameter is `MacroCallMetadata`",
        ),
        (
            "macro @ignored: Item -> Item = item => item\nmacro ordinary: Expr -> Expr = value => parse_quote { $value }\n@ignored ordinary\n",
            "macro `ordinary` has no overload whose first parameter is `MacroCallMetadata`",
        ),
        (
            "macro invalid = metadata: MacroCallMetadata => { let visibility = metadata.visibility; parse_quote { $visibility let (left, right) = (1, 2) } }\npub invalid\n",
            "visibility may only be spliced onto `let`, `def`, `type`, `extern`, or `trait` declarations",
        ),
        (
            "type Number wrap pub I32\n",
            "representation visibility cannot exceed the type's visibility",
        ),
        (
            "let visibility = Private\n",
            "`Private` syntax values are compile-time-only",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid visibility syntax should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn implicit_macro_call_metadata_can_make_overloads_ambiguous() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let source = concat!(
        "macro choose: Expr -> Expr = value => parse_quote { $value }\n",
        "macro choose: (MacroCallMetadata; Expr) -> Expr = (metadata; value) => parse_quote { $value }\n",
        "let result = choose 1\n",
    );
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(&with_syntax_imports(source), root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("equal-consumption overloads should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `choose`")
    );
}

#[test]
fn diagnoses_invalid_modifier_definitions_and_applications() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "macro @invalid: (SyntaxNode; Item) -> Item = (value; item) => item\n",
            "modifier macro `@invalid` must have signature `Item -> Item`, `Item -> Sequence<Item>`, or `Item -> Syntax`, optionally with a leading `Parenthesized<Expr | Type | Pattern> ->` argument",
        ),
        (
            "macro @required: (Parenthesized<Expr>; Item) -> Item = (value; item) => item\n@required\nlet value = 1\n",
            "modifier macro `@required` requires a parenthesized argument",
        ),
        (
            "macro @plain: Item -> Item = item => item\n@plain(1)\nlet value = 1\n",
            "modifier macro `@plain` does not accept an argument",
        ),
        (
            "macro ordinary = value => parse_quote { $value }\n@ordinary\nlet value = 1\n",
            "macro `ordinary` is function-style and cannot be used as modifier `@ordinary`",
        ),
        (
            "macro @recursive_constructor: Item -> Item = item => item\n",
            "modifier name `@recursive_constructor` is reserved by the compiler",
        ),
        (
            "macro @doc: Item -> Item = item => item\n",
            "modifier name `@doc` is reserved by the compiler",
        ),
        (
            "@doc\ntype Documented wrap I32\n",
            "`@doc` requires a parenthesized string literal",
        ),
        (
            "@doc(42)\ntype Documented wrap I32\n",
            "`@doc` requires a string literal argument",
        ),
        (
            "@doc(\"bad\\q\")\ntype Documented wrap I32\n",
            "unknown string escape `\\q`",
        ),
        (
            "@doc(\"not named\")\nuse std.core.*\n",
            "`@doc` may only modify a named declaration",
        ),
        (
            "/// not named\n42\n",
            "`@doc` may only modify a named declaration",
        ),
        (
            "macro @identity: Item -> Item = item => item\n@identity\nuse std.core.*\n",
            "modifier macros may only be applied to `let`, `def`, `type`, `extern`, `trait`, or `impl` items",
        ),
        (
            "macro @recurse: Item -> Item = _ => parse_quote { @recurse let value = 1 }\n@recurse\nlet original = 0\n",
            "recursive modifier macro expansion of `@recurse`",
        ),
    ] {
        let loaded = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root);
        let program = match loaded {
            Ok(program) => program,
            Err(error)
                if expected == "`@doc` may only modify a named declaration"
                    && error.contains("documentation comments require a named declaration") =>
            {
                continue;
            }
            Err(error) => panic!("source should parse: {error}"),
        };
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid modifier use should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn accepts_opaque_type_and_pattern_macro_inputs_and_contextual_splices() {
    let module = type_check(concat!(
        "def type_identity: Type -> Type = value => value\n",
        "def pattern_identity: Pattern -> Pattern = value => value\n",
        "macro define_value = (ty: Type; value: Expr) => {\n",
        "    let actual = type_identity ty\n",
        "    parse_quote { let generated: $actual = $value }\n",
        "}\n",
        "macro destructure = (pattern: Pattern; value: Expr) => {\n",
        "    let actual = pattern_identity pattern\n",
        "    parse_quote { let $actual = $value }\n",
        "}\n",
        "define_value (I32 -> I32) (value => value)\n",
        "destructure ((left, right)) (40, 2)\n",
        "let answer: I32 = generated (left + right)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("type and pattern splices should generate valid code");
}

#[test]
fn accepts_product_type_and_pattern_macro_inputs_without_extra_grouping() {
    let module = type_check(concat!(
        "macro for = (pattern: Pattern; _: Ident<\"in\">; value: Expr; body: Expr) => parse_quote {\n",
        "    { let $pattern = $value; $body }\n",
        "}\n",
        "macro ascribe = (ty: Type; value: Expr) => parse_quote { $ty :: $value }\n",
        "let direct: I32 = for (left, right) in (40, 2) { left + right }\n",
        "let legacy: I32 = for ((left, right)) in (40, 2) { left + right }\n",
        "let empty: () = for () in () { () }\n",
        "let pair: (I32, String) = ascribe (I32, String) (1, \"value\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("product category arguments should not require extra grouping");
}

#[test]
fn splices_types_and_patterns_through_expression_quotation_contexts() {
    let module = type_check(concat!(
        "macro typed_identity = ty: Type => parse_quote { ($ty -> $ty) :: (value => value) }\n",
        "macro parameter_function = pattern: Pattern => parse_quote { $pattern => 42 }\n",
        "macro matching = pattern: Pattern => parse_quote { match (Bool :: True) { $pattern => 1, _ => 0 } }\n",
        "let identity: I32 -> I32 = typed_identity I32\n",
        "let constant: I32 -> I32 = parameter_function (_)\n",
        "let matched: I32 = matching True\n",
        "let result: I32 = identity (constant matched)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("contextual type and pattern splices should compile in expressions");
}

#[test]
fn quote_uses_contextual_results_and_reinterprets_opaque_fragments() {
    let module = type_check(concat!(
        "macro delayed: Expr -> Expr = value => {\n",
        "    let fragment: Syntax = quote { $value + 1 }\n",
        "    Expr :: parse_quote { $fragment }\n",
        "}\n",
        "macro raw: Expr -> Syntax = value => quote { $value }\n",
        "macro pattern_result: Expr -> Expr = value => {\n",
        "    let pattern: Pattern = parse_quote { Some inner }\n",
        "    parse_quote { match Some $value { $pattern => inner } }\n",
        "}\n",
        "let direct: I32 = raw 1\n",
        "let result: I32 = delayed (pattern_result 40) + direct\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("opaque syntax should reparse according to its later quote context");
}

#[test]
fn quote_never_validates_or_reinterprets_its_result_unlike_parse_quote() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    Expr :: parse_quote { let generated = 1 }\n",
                "}\n",
                "let result = invalid ()\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("an item template is not a valid `Expr`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "quotation cannot be interpreted as Expr" })
    );

    resolve(concat!(
        "macro always_syntax: Expr -> Expr = _: Expr => {\n",
        "    let fragment: Syntax = Expr :: quote { let generated = 1 }\n",
        "    parse_quote { 0 }\n",
        "}\n",
        "let result = always_syntax ()\n",
    ));
}

#[test]
fn quote_result_excludes_syntax_which_remains_quotes_alone() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    for source in [
        concat!(
            "macro invalid: Expr -> Expr = value => {\n",
            "    let fragment: Syntax = parse_quote { $value }\n",
            "    Expr :: parse_quote { $fragment }\n",
            "}\n",
            "let result = invalid (1)\n",
        ),
        concat!(
            "macro invalid = value => Syntax :: parse_quote { $value }\n",
            "let result = invalid (1)\n",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("`Syntax` is `quote`'s result, not a `parse_quote` target");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.message
                == "Syntax is not a supported `parse_quote` context"),
            "{diagnostics:#?}",
        );
    }
}

#[test]
fn parse_quote_without_a_contextual_type_is_an_error() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "def helper = () => parse_quote { 1 }\n",
                "macro bad = _: Expr => helper ()\n",
                "bad 1\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("an untyped helper leaves `parse_quote` without a contextual type");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .starts_with("`parse_quote` requires a contextual syntax type")
        }),
        "{diagnostics:#?}",
    );
}

#[test]
fn macro_declaring_a_concrete_syntax_result_rejects_a_bare_quote_tail() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("macro invalid: Expr -> Expr = value => quote { $value }\n"),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("`quote` always returns `Syntax`, not the declared `Expr` result");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message
                == "macro `invalid` declares result `Expr`, but its body ends in `quote`, which always returns opaque `Syntax`; use `parse_quote` instead"
        }),
        "{diagnostics:#?}",
    );
}

#[test]
fn an_unannotated_macro_ending_in_bare_quote_infers_syntax_not_its_contents_shape() {
    // The macro's own inferred result must come from `quote`'s fixed
    // contract, not from what its contents happen to parse as — otherwise
    // every bare `quote { <expression> }` tail would wrongly look like it
    // declared `Expr`.
    resolve(concat!(
        "macro identity = value: Expr => quote { $value }\n",
        "let result = identity 1\n",
    ));
}

#[test]
fn opaque_syntax_captures_whole_delimiter_contents_and_is_the_broadest_overload() {
    let module = type_check(concat!(
        "macro capture: Braced<Syntax> -> Expr = body => match body {\n",
        "    Braced fragment => Expr :: parse_quote { $fragment },\n",
        "}\n",
        "macro choose: Syntax -> Expr = _: Syntax => parse_quote { 1 }\n",
        "macro choose: SyntaxNode -> Expr = _: SyntaxNode => parse_quote { 2 }\n",
        "macro accepts: Braced<Syntax> -> Expr = _: Braced<Syntax> => parse_quote { 3 }\n",
        "let captured: I32 = capture { 40 + 2 }\n",
        "let selected: I32 = choose name\n",
        "let arbitrary: I32 = accepts { left, => right }\n",
        "let result: I32 = captured + selected + arbitrary\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("whole-fragment capture and SyntaxNode overload precedence should compile");
}

#[test]
fn syntax_node_quotation_requires_one_shortest_structural_node() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    let node: SyntaxNode = parse_quote { left right }\n",
                "    parse_quote { 0 }\n",
                "}\n",
                "let result = invalid ()\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("two shortest nodes must not satisfy SyntaxNode");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "quotation does not contain exactly one structural syntax node"
    }));
}

#[test]
fn contextual_item_sequence_quotes_flatten_empty_single_and_multiple_results() {
    let module = type_check(concat!(
        "macro empty: Expr -> Sequence<Item> = _: Expr => parse_quote {}\n",
        "macro single: Expr -> Sequence<Item> = _: Expr => parse_quote { let one: I32 = 1 }\n",
        "macro multiple: Expr -> Sequence<Item> = _: Expr => parse_quote {\n",
        "    let two: I32 = 2\n",
        "    let three: I32 = 3\n",
        "}\n",
        "empty ()\n",
        "single ()\n",
        "multiple ()\n",
        "let result: I32 = one + two + three\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("contextual item sequences should flatten in source order");
}

#[test]
fn generic_user_macros_and_unsupported_parse_quote_contexts_are_rejected() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("macro generic: <T> Expr -> T = value => value\n", root)
        .expect("generic macro syntax should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("generic user macros remain unsupported");
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message == "generic user-defined macros are not supported"));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    let ident: Ident<String> = parse_quote { name }\n",
                "    parse_quote { 0 }\n",
                "}\n",
                "let result = invalid ()\n",
            ),
            root,
        )
        .expect("unsupported contextual quote source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("narrow syntax nodes are not supported parse_quote contexts");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "Ident<String> is not a supported `parse_quote` context"
    }));
}

#[test]
fn type_and_pattern_macro_inputs_support_atomic_and_compound_forms() {
    let module = type_check(concat!(
        "macro atomic_type = _: Type => parse_quote { let atomic: I32 = 1 }\n",
        "macro applied_type = _: Type => parse_quote { let applied: I32 = 2 }\n",
        "macro nominal_pattern = _: Pattern => parse_quote { let nominal: I32 = 3 }\n",
        "atomic_type I32\n",
        "applied_type (Shared<I32>)\n",
        "nominal_pattern (Some value)\n",
        "let result: I32 = atomic + applied + nominal\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("atomic and grouped category arguments should compile");
}

#[test]
fn type_and_pattern_overlaps_with_expression_overloads_are_ambiguous() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        concat!(
            "macro choose = _: Expr => parse_quote { 1 }\n",
            "macro choose = _: Type => parse_quote { 2 }\n",
            "let result = choose I32\n",
        ),
        concat!(
            "macro choose = _: Type => parse_quote { 1 }\n",
            "macro choose = _: Pattern => parse_quote { 2 }\n",
            "let result = choose (I32)\n",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("overlapping syntax categories should be ambiguous");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `choose`")
        );
    }
}

#[test]
fn diagnoses_invalid_type_and_pattern_macro_inputs_and_splices() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "macro consume = _: Type => parse_quote { 1 }\nlet result = consume (I32 ->)\n",
            "argument 1 of macro `consume` must be a type",
        ),
        (
            "macro invalid = value: Pattern => parse_quote { let generated: $value = 1 }\ninvalid name\n",
            "type splice `$value` contains pattern syntax",
        ),
        (
            "macro invalid = value: Type => parse_quote { let $value = 1 }\ninvalid I32\n",
            "pattern splice `$value` contains type syntax",
        ),
        (
            "def runtime = value => value\nlet result = runtime (I32 -> I32)\n",
            "grouped type or pattern syntax requires a matching macro parameter",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse losslessly");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid category syntax should fail expansion or resolution");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn type_and_pattern_overloads_are_more_specific_than_syntax() {
    let module = type_check(concat!(
        "macro choose_type = _: SyntaxNode => parse_quote { let type_choice: String = \"syntax\" }\n",
        "macro choose_type = _: Type => parse_quote { let type_choice: I32 = 1 }\n",
        "macro choose_pattern = _: SyntaxNode => parse_quote { let pattern_choice: String = \"syntax\" }\n",
        "macro choose_pattern = _: Pattern => parse_quote { let pattern_choice: I32 = 2 }\n",
        "choose_type I32\n",
        "choose_pattern (Some value)\n",
        "let result: I32 = type_choice + pattern_choice\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("category overloads should beat Syntax");
}

#[test]
fn expands_macros_and_splices_inside_generated_items() {
    let module = type_check(concat!(
        "macro expression_identity = value: Expr => parse_quote { $value }\n",
        "macro define_value = value: Expr => parse_quote {\n",
        "    let generated: I32 = expression_identity $value\n",
        "}\n",
        "define_value 42\n",
        "let result: I32 = generated\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nested macros in generated items should expand");
}

#[test]
fn generates_extern_trait_and_implementation_items() {
    let module = type_check(concat!(
        "macro define_extern = _: Expr => parse_quote {\n",
        "    extern \"c\" { generated_external: I32 -> I32 }\n",
        "}\n",
        "macro define_trait = _: Expr => parse_quote {\n",
        "    trait GeneratedTrait T { transform: T -> T }\n",
        "}\n",
        "macro define_impl = replacement: Expr => parse_quote {\n",
        "    impl GeneratedTrait I32 { transform = value => $replacement }\n",
        "}\n",
        "define_extern ()\n",
        "define_trait ()\n",
        "define_impl 41\n",
        "let transformed: I32 = GeneratedTrait.transform 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generated resolver-safe declaration items should generate code");
}

#[test]
fn generates_generic_conditional_trait_implementation_items() {
    let module = type_check(concat!(
        "trait Bound T { check: T -> Bool }\n",
        "trait Target T { act: move T -> T }\n",
        "impl Bound I32 { check = value => True }\n",
        "macro define_impl = _: Expr => parse_quote {\n",
        "    impl <T where Bound T> Target T { act = move value => value }\n",
        "}\n",
        "define_impl ()\n",
        "let answer: I32 = Target.act 41\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a macro-generated conditional trait implementation should generate code");
}

#[test]
fn generates_traits_with_associated_types() {
    let module = type_check(concat!(
        "macro define_trait = _: Expr => parse_quote {\n",
        "    trait Generated Input { type Output; generate: Input -> Output }\n",
        "}\n",
        "define_trait ()\n",
        "impl Generated I32 { type Output = String; generate = value => \"generated\" }\n",
        "let generated = Generated.generate 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generated functional dependencies should resolve and specialize");
}

#[test]
fn diagnoses_invalid_item_macro_outputs_and_placements() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "macro invalid: Expr -> Item = value => parse_quote { $value }\ninvalid 1\n",
            "quotation cannot be interpreted as Item",
        ),
        (
            "macro emit = value: Expr => parse_quote { let generated = $value }\nlet result = emit 1\n",
            "macro `emit` produces item syntax and may only be invoked as a standalone top-level item",
        ),
        (
            "macro emit: Expr -> Item = value => parse_quote { let generated = $value }\ndef enclosing = () => { emit 1; () }\n",
            "macro `emit` produces item syntax and may only be invoked as a standalone top-level item",
        ),
        (
            "macro emit = value: Expr => parse_quote { let generated = $value }\nemit 1 2\n",
            "item-producing macro `emit` cannot have excess arguments",
        ),
        (
            "macro emit = _: Expr => parse_quote { macro generated = value => parse_quote { $value } }\nemit ()\n",
            "item quotations cannot generate `macro` declarations yet",
        ),
        (
            "macro consume = value: Item => parse_quote { 1 }\nconsume candidate\n",
            "function-style macro `consume` cannot accept `Item`; define a modifier macro with `macro @consume` instead",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid item macro use should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn evaluates_pure_syntax_helpers_and_conditional_macros() {
    let module = type_check(concat!(
        "def syntax_identity: SyntaxNode -> SyntaxNode = value => value\n",
        "macro choose = (condition; then; else) => parse_quote {\n",
        "    match $condition { True => $then, False => $else, }\n",
        "}\n",
        "macro passthrough = value => syntax_identity value\n",
        "let condition: Bool = True\n",
        "let result: I32 = passthrough (choose condition 1 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("expanded macros should generate code");
}

#[test]
fn expands_typed_macros_with_literal_identifier_parameters() {
    let module = type_check(concat!(
        "macro conditional =\n",
        "    (condition: Expr;\n",
        "    then_branch: Expr;\n",
        "    _: Ident<\"else\">;\n",
        "    else_branch: Expr) => parse_quote {\n",
        "        match $condition {\n",
        "            True() => $then_branch,\n",
        "            False() => $else_branch,\n",
        "        }\n",
        "    }\n",
        "let condition: Bool = True\n",
        "let result: I32 = conditional condition 1 else 2\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("typed macro should expand to an ordinary expression");
}

#[test]
fn expands_macros_with_delimited_syntax_parameters() {
    let module = type_check(concat!(
        "macro pair = _: Parenthesized<(Ident<String>; Ident<String>)> => parse_quote { 11 }\n",
        "macro names = _: Bracketed<Sequence<Ident<String>>> => parse_quote { 22 }\n",
        "macro body = _: Braced<Sequence<SyntaxNode>> => parse_quote { 33 }\n",
        "let pair_result: I32 = pair (left right)\n",
        "let empty_names: I32 = names []\n",
        "let names_result: I32 = names [one two three]\n",
        "let body_result: I32 = body { left (right nested) }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("delimited macro parameters should expand");
}

#[test]
fn delimited_macro_overloads_use_structural_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Expr => parse_quote { 1 }\n",
        "macro classify = _: Parenthesized<Sequence<SyntaxNode>> => parse_quote { 2 }\n",
        "macro classify = _: Parenthesized<Sequence<Ident<String>>> => parse_quote { 3 }\n",
        "macro classify = _: Parenthesized<(Ident<String>; Ident<String>)> => parse_quote { 4 }\n",
        "let fixed: I32 = classify (left right)\n",
        "let sequence: I32 = classify (left)\n",
        "let empty: I32 = classify ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("delimiter overloads should be ordered by structural specificity");
}

#[test]
fn constructs_and_destructures_delimited_syntax_values() {
    let module = type_check(concat!(
        "macro inspect = value: Parenthesized<Sequence<Ident<String>>> => match value {\n",
        "    Parenthesized (Sequence (Ident \"only\")) => parse_quote { 1 },\n",
        "    _ => parse_quote { 0 },\n",
        "}\n",
        "macro construct = _: Expr => Parenthesized (parse_quote { 7 })\n",
        "macro construct_empty = _: Expr => Parenthesized (Sequence ())\n",
        "macro construct_sequence = _: Expr => Parenthesized (Sequence (parse_quote { increment }, parse_quote { 41 }))\n",
        "macro construct_braced = _: Expr => Braced (Sequence (parse_quote { 9 }))\n",
        "macro preserve = value: Parenthesized<Sequence<SyntaxNode>> => value\n",
        "def increment: I32 -> I32 = value => value + 1\n",
        "let inspected: I32 = inspect (only)\n",
        "let constructed: I32 = construct ()\n",
        "let empty = construct_empty ()\n",
        "let ordered: I32 = construct_sequence ()\n",
        "let braced: I32 = construct_braced ()\n",
        "let preserved: I32 = preserve ( 44 )\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("delimiter values should support constructors and nominal patterns");
}

#[test]
fn rejects_invalid_sequence_positions_and_source_punctuation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Separated<Ident<String>, Comma> => parse_quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("bare Separated should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "`Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`"
    }));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Braced<Sequence<Syntax>> => parse_quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("opaque Syntax has no canonical sequence partition");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .starts_with("opaque `Syntax` may be a top-level argument")
    }));
}

#[test]
fn top_level_macro_sequences_capture_zero_one_and_many_arguments() {
    let module = type_check(concat!(
        "macro count = (_: Ident<\"marker\">; values: Sequence<Ident<String>>; _: Equals; name: Ident<String>; _: FatArrow; _: Braced<Syntax>) => match values {\n",
        "    Sequence () => quote { let $name: I32 = 0 },\n",
        "    Sequence (first: Ident<String>, rest: Sequence<Ident<String>>) => match rest {\n",
        "        Sequence () => quote { let $name: I32 = 1 },\n",
        "        _ => quote { let $name: I32 = 3 },\n",
        "    },\n",
        "}\n",
        "count marker = zero => {}\n",
        "count marker alpha = one => {}\n",
        "count marker alpha beta gamma = three => {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("top-level macro sequences should compile");
}

#[test]
fn a_modules_own_macro_is_not_hidden_by_a_prelude_value_of_the_same_name() {
    // Regression test: the prelude installed its values as compile-time
    // helpers, which the expander looks up before macros, so the prelude's
    // `panic` function hid this module's `panic` macro. A macro named like an
    // iterator operation leaves the blanket member usable in a pipe.
    let module = type_check(concat!(
        "macro panic: Expr -> Expr = value => parse_quote { $value + 1 }\n",
        "macro count: Expr -> Expr = value => parse_quote { $value * 2 }\n",
        "let answer: I32 = panic 41\n",
        "let doubled: I32 = count 21\n",
        "let evens = (0 .. 6) |> filter (x => x / 2 * 2 == x) |> count\n",
    ));
    lower(&module);
}

#[test]
fn top_level_macro_sequences_backtrack_for_visibility_and_fixed_suffixes() {
    let module = type_check(concat!(
        "macro classify = (_: Ident<\"marker\">;\n",
        "    values: Sequence<Ident<String>>;\n",
        "    visibility: Visibility;\n",
        "    _: Equals;\n",
        "    name: Ident<String>; _: FatArrow; _: Braced<Syntax>) => match (values, visibility) {\n",
        "        (Sequence (), Private) => quote { let $name: I32 = 40 },\n",
        "        (Sequence (first: Ident<String>, rest: Sequence<Ident<String>>), Public) => quote { let $name: I32 = 41 },\n",
        "        (Sequence (first: Ident<String>, rest: Sequence<Ident<String>>), Package) => quote { let $name: I32 = 42 },\n",
        "        _ => quote { let $name: I32 = 43 },\n",
        "    }\n",
        "classify marker = private_value => {}\n",
        "classify marker alpha beta pub = public_value => {}\n",
        "classify marker alpha pub(package) = package_value => {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("sequence suffix matching should handle implicit and explicit visibility");
}

#[test]
fn top_level_macro_sequence_may_be_the_final_parameter() {
    let module = type_check(concat!(
        "macro tail = (_: Ident<\"marker\">; name: Ident<String>; values: Sequence<Ident<String>>) => match values {\n",
        "    Sequence () => quote { let $name: I32 = 0 },\n",
        "    Sequence (first: Ident<String>, rest: Sequence<Ident<String>>) => match rest {\n",
        "        Sequence () => quote { let $name: I32 = 1 },\n",
        "        _ => quote { let $name: I32 = 3 },\n",
        "    },\n",
        "}\n",
        "tail marker zero\n",
        "tail marker one alpha\n",
        "tail marker three alpha beta gamma\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a trailing top-level sequence should greedily capture the remaining arguments");
}

#[test]
fn top_level_macro_sequence_continues_onto_indented_lines() {
    let module = type_check(concat!(
        "macro tail = (_: Ident<\"marker\">; name: Ident<String>; values: Sequence<Ident<String>>) => match values {\n",
        "    Sequence () => quote { let $name: I32 = 0 },\n",
        "    Sequence (first: Ident<String>, rest: Sequence<Ident<String>>) => match rest {\n",
        "        Sequence () => quote { let $name: I32 = 1 },\n",
        "        _ => quote { let $name: Bool = True },\n",
        "    },\n",
        "}\n",
        "tail marker wrapped alpha\n",
        "    beta\n",
        "    gamma\n",
        "tail marker single alpha\n",
        "let wrapped_is_many: Bool = wrapped\n",
        "let single_is_one: I32 = single\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("an indented line should extend a trailing top-level sequence");
}

#[test]
fn longer_complete_overload_still_beats_a_trailing_sequence_overload() {
    let module = type_check(concat!(
        "macro pick = (value: Ident<String>; _: Sequence<Ident<String>>) => quote { let $value: I32 = 0 }\n",
        "macro pick = (value: Ident<String>; _: Sequence<Ident<String>>; _: Equals; _: Braced<Syntax>) => quote { let $value: I32 = 1 }\n",
        "pick alpha one two\n",
        "pick beta one two = {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("the longer overload wins when its suffix is present, the trailing sequence matches otherwise");
}

#[test]
fn fixed_and_more_specific_overloads_beat_top_level_sequences() {
    let module = type_check(concat!(
        "macro choose = (_: Sequence<Expr>; _: Equals; name: Ident<String>; _: FatArrow; _: Braced<Syntax>) => quote { let $name: String = \"wrong\" }\n",
        "macro choose = (_: Sequence<Ident<String>>; _: Equals; name: Ident<String>; _: FatArrow; _: Braced<Syntax>) => quote { let $name: I32 = 1 }\n",
        "macro choose = (_: Ident<String>; _: Equals; name: Ident<String>; _: FatArrow; _: Braced<Syntax>) => quote { let $name: I32 = 2 }\n",
        "choose value = fixed => {}\n",
        "choose left right = repeated => {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("fixed and category-specific overloads should win");
}

#[test]
fn annotated_top_level_sequences_compile_and_incomparable_sequences_are_ambiguous() {
    let module = type_check(concat!(
        "macro annotated: (Sequence<Ident<String>>; Equals; Ident<String>; FatArrow; Braced<Syntax>) -> Syntax =\n",
        "    (values: Sequence<Ident<String>>; _: Equals; name: Ident<String>; _: FatArrow; _: Braced<Syntax>) => quote { let $name: I32 = 42 }\n",
        "annotated first second = generated => {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("annotated top-level sequences should compile");

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro clash = (_: Sequence<Type>; _: Equals; _: Ident<String>; _: FatArrow; _: Braced<Syntax>) => quote { let generated: I32 = 1 }\n",
                "macro clash = (_: Sequence<Pattern>; _: Equals; _: Ident<String>; _: FatArrow; _: Braced<Syntax>) => quote { let generated: I32 = 2 }\n",
                "clash Value = output => {}\n",
            )),
            root,
        )
        .expect("ambiguous macro source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("incomparable repeated categories should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `clash`")
    );
}

#[test]
fn rejects_invalid_top_level_macro_sequence_signatures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            "macro invalid = (first: Sequence<Ident<String>>; second: Sequence<Expr>; _: Equals) => parse_quote { 0 }\n",
            "a macro signature may contain at most one top-level `Sequence` parameter",
        ),
        (
            "macro @invalid: (Sequence<Ident<String>>; Item) -> Item = (values; item) => item\n",
            "top-level `Sequence` parameters are not supported by modifier macros",
        ),
        (
            "macro invalid = (values: Sequence<Sequence<Ident<String>>>; _: Equals) => parse_quote { 0 }\n",
            "a top-level `Sequence` element must be a single syntax category, found `Sequence<Ident<String>>`",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("invalid macro declaration should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid top-level sequence should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected {expected:?}, found {diagnostics:#?}",
        );
    }
}

#[test]
fn rejects_curried_macro_annotations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(
                "macro invalid: Expr -> Expr -> Item = (left; right) => parse_quote { $left }\n",
            ),
            root,
        )
        .expect("a curried macro annotation should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("a curried macro annotation should be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message
            == "a macro annotation must accept one or more syntax-category parameters and \
                return a syntax category"),
        "expected the curried annotation to be rejected, found {diagnostics:#?}",
    );
}

#[test]
fn matches_comma_and_separated_delimited_syntax() {
    let module = type_check(concat!(
        "macro fixed = _: Parenthesized<(Ident<String>; Comma; Ident<String>)> => parse_quote { 1 }\n",
        "macro product = _: Parenthesized<(Ident<String>, Ident<String>)> => parse_quote { 7 }\n",
        "macro separated = _: Parenthesized<Separated<Ident<String>, Comma>> => parse_quote { 2 }\n",
        "macro bracketed = _: Bracketed<Separated<Ident<String>, Comma>> => parse_quote { 5 }\n",
        "macro braced = _: Braced<Separated<Ident<String>, Comma>> => parse_quote { 6 }\n",
        "macro syntax = value: Parenthesized<Sequence<SyntaxNode>> => match value {\n",
        "    Parenthesized (Sequence (Ident \"left\", Comma, Ident \"right\")) => parse_quote { 3 },\n",
        "    _ => parse_quote { 0 },\n",
        "}\n",
        "macro comma = value: Parenthesized<Comma> => match value {\n",
        "    Parenthesized Comma => parse_quote { 4 },\n",
        "}\n",
        "let fixed_result: I32 = fixed (left, right)\n",
        "let product_result: I32 = product (left, right)\n",
        "let product_trailing: I32 = product (left, right,)\n",
        "let empty: I32 = separated ()\n",
        "let single: I32 = separated (one)\n",
        "let multiple: I32 = separated (one, two, three)\n",
        "let trailing: I32 = separated (one, two,)\n",
        "let structural: I32 = syntax (left, right)\n",
        "let comma_result: I32 = comma (,)\n",
        "let bracketed_result: I32 = bracketed [left, right,]\n",
        "let braced_result: I32 = braced {left, right}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("comma and separated syntax should match");
}

#[test]
fn separated_overloads_use_structural_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Parenthesized<Sequence<SyntaxNode>> => parse_quote { 1 }\n",
        "macro classify = _: Parenthesized<Separated<Ident<String>, Comma>> => parse_quote { 2 }\n",
        "macro classify = _: Parenthesized<(Ident<String>; Comma; Ident<String>)> => parse_quote { 3 }\n",
        "let fixed: I32 = classify (one, two)\n",
        "let separated: I32 = classify (one, two, three)\n",
        "let empty: I32 = classify ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("fixed, separated, and ordinary sequence overloads should be ordered");
}

#[test]
fn matches_product_macro_parameters() {
    let module = type_check(concat!(
        "macro unit = () => parse_quote { 1 }\n",
        "macro sum = (a: Expr; b: Expr, c: Expr; d: Expr) => parse_quote { $a + $b + $c + $d }\n",
        "macro product: (Expr, Expr) -> Expr = (left, right) => parse_quote { $left * $right }\n",
        "macro nested = value: Parenthesized<(Ident<String>; (Expr, Expr))> => match value {\n",
        "    Parenthesized (_; (left, right)) => parse_quote { $left - $right },\n",
        "}\n",
        "macro listed = value: Bracketed<(Expr, Expr)> => match value {\n",
        "    Bracketed (left, right) => parse_quote { $left + $right },\n",
        "}\n",
        "let unit_result: I32 = unit ()\n",
        "let sum_result: I32 = sum 1 (2, 3) 4\n",
        "let product_result: I32 = product (6, 7)\n",
        "let nested_result: I32 = nested (pair (9, 4))\n",
        "let listed_result: I32 = listed [5, 6]\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("product macro parameters should match comma-separated syntax");
}

#[test]
fn product_macro_parameters_reject_juxtaposed_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (signature, argument) in [
        ("_: (Expr, Expr)", "(1 2)"),
        ("_: (Expr, Expr)", "(1, 2, 3)"),
        ("_: (Expr, Expr)", "1"),
        ("()", "1"),
        ("_: Parenthesized<(Expr, Expr)>", "(1 2)"),
        ("_: Parenthesized<(Expr; Expr)>", "(1, 2)"),
    ] {
        let source = format!(
            "use std.syntax.(Expr, Parenthesized, parse_quote)\n\
             macro shape = {signature} => parse_quote {{ 0 }}\n\
             let value: I32 = shape {argument}\n"
        );
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, root)
            .expect("source should parse");
        NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("mismatched product syntax should be rejected");
    }
}

#[test]
fn constructs_and_destructures_separated_syntax() {
    let module = type_check(concat!(
        "macro inspect = value: Parenthesized<Separated<Ident<String>, Comma>> => match value {\n",
        "    Parenthesized (Separated (separator: Comma, elements: (Ident<\"one\">, Ident<\"two\">), trailing: True<()>)) => parse_quote { 1 },\n",
        "}\n",
        "macro construct = _: Expr => Parenthesized (Separated (\n",
        "    separator: Comma,\n",
        "    elements: (parse_quote { 10 }, parse_quote { 20 }),\n",
        "    trailing: True,\n",
        "))\n",
        "let inspected: I32 = inspect (one, two,)\n",
        "let constructed: (I32, I32) = construct ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("separated syntax should support construction and patterns");
}

#[test]
fn rejects_malformed_separated_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for argument in ["(,)", "(, one)", "(one,, two)", "(one two)", "(one, 2)"] {
        let source = format!(
            "macro names = _: Parenthesized<Separated<Ident<String>, Comma>> => parse_quote {{ 0 }}\nlet value: I32 = names {argument}\n"
        );
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, root)
            .expect("source should parse");
        NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("malformed separated syntax should be rejected");
    }

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro invalid = _: Expr => Parenthesized (Separated (\n",
                "    separator: Comma, elements: (), trailing: True,\n",
                "))\n",
                "let value = invalid ()\n",
            ),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("an empty separated value cannot have a trailing comma");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "an empty `Separated` value cannot have a trailing separator"
    }));
}

#[test]
fn expands_standard_braced_when_clauses() {
    let module = type_check(concat!(
        "let condition: Bool = True\n",
        "let clauses: I32 = when {\n",
        "  False => 7,\n",
        "  condition => 8,\n",
        "  else => 9,\n",
        "}\n",
        "let clauses_without_else = when { False => () }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("standard braced when should expand and generate code");
}

#[test]
fn requires_else_to_be_the_last_braced_when_clause() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let invalid = when { else => (), True => () }\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("a non-final else clause should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "macro `when` panicked: when clauses after else are unsupported"
    }));
}

#[test]
fn expands_if_else_if_chain() {
    let module = type_check(concat!(
        "let a: Bool = True\n",
        "let b: Bool = False\n",
        "let plain: I32 = if a { 1 } else { 2 }\n",
        "let bare: I32 = if a 1 else 2\n",
        "let chained: I32 = if a { 1 } else if b { 2 } else { 3 }\n",
        "let no_else: () = if b { () }\n",
        "let nested: I32 = if a { 1 } else (if b { 2 } else { 3 })\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("if/else if/else should expand and generate code");
}

#[test]
fn expands_standard_while_with_loop_control() {
    let module = type_check(concat!(
        "def run = () => {\n",
        "  let mut keep_going: Bool = True\n",
        "  while keep_going { keep_going = False; continue }\n",
        "}\n",
        "run ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("standard while should expand and generate valid LLVM");
    assert!(llvm.contains("loop.body"));
    assert!(llvm.contains("loop.exit"));
}

#[test]
fn expands_standard_for_over_ranges_and_product_iterators() {
    let module = type_check(concat!(
        "pub type PairIterator wrap pub (current: I32, end: I32)\n",
        "impl Iterator PairIterator { type Item = (I32, I32);\n",
        "  next = move PairIterator (current, end) => match current < end {\n",
        "    True() => IterStep.Yield ((current, current + 10), PairIterator (current + 1, end)),\n",
        "    False() => IterStep.Done (PairIterator (current, end)),\n",
        "  }\n",
        "}\n",
        "impl IntoIterator PairIterator { type Iter = PairIterator; into_iterator = move iterator => iterator }\n",
        "def run = () => {\n",
        "  let mut total = 0\n",
        "  for value in (0 ..= 4) {\n",
        "    match value == 2 { True() => { continue }, False() => () }\n",
        "    total = total + value\n",
        "  }\n",
        "  for (left, right) in (PairIterator (0, 2)) {\n",
        "    total = total + left + right\n",
        "  }\n",
        "  total\n",
        "}\n",
        "let result: I32 = run ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("for loops over ranges and custom iterators should generate LLVM");
    assert!(llvm.contains("loop.body"));
    assert!(llvm.contains("trait.call"));
}

#[test]
fn provides_integer_range_iterator_implementations() {
    type_check(concat!(
        "let i8: IterStep<Range<I8>, I8> = Iterator.next ((I8 :: 0) .. (I8 :: 1))\n",
        "let i16: IterStep<Range<I16>, I16> = Iterator.next ((I16 :: 0) .. (I16 :: 1))\n",
        "let i32: IterStep<Range<I32>, I32> = Iterator.next (0 .. 1)\n",
        "let i64: IterStep<Range<I64>, I64> = Iterator.next ((I64 :: 0) .. (I64 :: 1))\n",
        "let u8: IterStep<Range<U8>, U8> = Iterator.next ((U8 :: 0) .. (U8 :: 1))\n",
        "let u16: IterStep<Range<U16>, U16> = Iterator.next ((U16 :: 0) .. (U16 :: 1))\n",
        "let u32: IterStep<Range<U32>, U32> = Iterator.next ((U32 :: 0) .. (U32 :: 1))\n",
        "let u64: IterStep<Range<U64>, U64> = Iterator.next ((U64 :: 0) .. (U64 :: 1))\n",
        "let isize: IterStep<Range<ISize>, ISize> = Iterator.next ((ISize :: 0) .. (ISize :: 1))\n",
        "let usize: IterStep<Range<USize>, USize> = Iterator.next ((USize :: 0) .. (USize :: 1))\n",
        "let inclusive: IterStep<RangeInclusive<U8>, U8> = Iterator.next ((U8 :: 255) ..= (U8 :: 255))\n",
    ));
}

#[test]
fn macro_overloads_choose_longest_then_most_specific() {
    let module = type_check(concat!(
        "macro select = value: Expr => parse_quote { 10 }\n",
        "macro select = (value: Expr; _: Ident<\"then\">; replacement: Expr) => parse_quote { $replacement }\n",
        "macro classify = value: SyntaxNode => parse_quote { 1 }\n",
        "macro classify = value: Expr => parse_quote { 2 }\n",
        "macro classify = value: Ident<String> => parse_quote { 3 }\n",
        "macro classify = _: Ident<\"else\"> => parse_quote { 4 }\n",
        "let longest: I32 = select 0 then 20\n",
        "let specific: I32 = classify else\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("selected overloads should generate code");
}

#[test]
fn rejects_braced_clause_syntax_for_if_now_that_it_belongs_to_when() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "let condition: Bool = True\nlet invalid = if { condition => 1, else => 2 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("the braced clause form is now `when`, not `if`");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message == "no overload of macro `if` matches this invocation"
        }),
        "{diagnostics:#?}"
    );
}

#[test]
fn diagnoses_duplicate_and_ambiguous_macro_overloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let duplicate = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro same = value: Expr => parse_quote { 1 }\n",
                "macro same = other: Expr => parse_quote { 2 }\n",
            ),
            root,
        )
        .expect("duplicate overload source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(duplicate)
        .expect_err_diagnostics("identical overload patterns should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate macro overload `same: Expr`")
    }));

    let ambiguous = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro crossed = (left: Ident<String>; right: Expr) => parse_quote { 1 }\n",
                "macro crossed = (left: Expr; right: Ident<String>) => parse_quote { 2 }\n",
                "crossed first second\n",
            ),
            root,
        )
        .expect("ambiguous overload source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(ambiguous)
        .expect_err_diagnostics("incomparable overloads should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "ambiguous invocation of macro `crossed`" })
    );
}

#[test]
fn rejects_a_mismatched_literal_identifier_macro_argument() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro conditional = (value: Expr; _: Ident<\"else\">) => parse_quote { $value }\n",
                "conditional 1 otherwise\n",
            ),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("literal identifier mismatch should fail expansion");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("argument 2 of macro `conditional` must be identifier `else`")
    }));
}

#[test]
fn rejects_bare_literal_identifier_macro_parameters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro conditional = (value: Expr; Ident \"else\") => parse_quote { $value }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("bare literal identifier parameters should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("parameters must match atomic syntax values")
    }));
}

#[test]
fn subtype_bound_call_preserves_string_literal_type() {
    let module = type_check(concat!(
        "def string_identity: <T where T <: String> move T -> T = <T where T <: String> move x => x\n",
        "string_identity \"foo\"\n",
    ));
    let item = &module.syntax().items[1];
    let Item::Expression(expression) = item else {
        panic!("expected expression item");
    };
    assert_eq!(
        module.type_of_expression(expression.syntax().id),
        Some(&CheckedType::StringLiteralSet(vec!["foo".to_owned()]))
    );
}

#[test]
fn subtype_bound_rejects_non_string_arguments() {
    let module = resolve(concat!(
        "def string_identity: <T where T <: String> move T -> T = <T where T <: String> move x => x\n",
        "string_identity 1\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("`I32` does not satisfy `T <: String`");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "subtype bound is not satisfied: `I32` is not a subtype of `String`"
    }));
}

#[test]
fn subtype_bound_reflexivity() {
    type_check(concat!(
        "def echo: <T where T <: T> move T -> T = <T where T <: T> move x => x\n",
        "echo \"hello\"\n",
        "echo 1\n",
    ));
}

#[test]
fn subtype_bound_union_introduction() {
    type_check(concat!(
        "def widen: <T where T <: I32 | String> T -> () = <T where T <: I32 | String> x => ()\n",
        "widen 1\n",
        "widen \"text\"\n",
    ));
}

#[test]
fn subtype_bound_union_elimination() {
    type_check(concat!(
        "def accept: <T where T <: I32 | String | F64> T -> () = <T where T <: I32 | String | F64> x => ()\n",
        "let value: I32 | String = 1\n",
        "accept value\n",
    ));
}

#[test]
fn ident_rejects_non_string_spelling_types() {
    let module = resolve(concat!(
        "type InvalidIdent = Ident<I32>\n",
        "def invalid: InvalidIdent\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("Ident's spelling argument must satisfy `Spelling <: String`");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "subtype bound is not satisfied: `I32` is not a subtype of `String`"
    }));
}

#[test]
fn default_type_bound_fills_omitted_type_argument() {
    let module = type_check(concat!(
        "type Box<T = String> = (value: T)\n",
        "let boxed: Box = (value: \"hi\")\n",
        "let explicit: Box<I32> = (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("defaulted and explicit instantiations should compile");
}

#[test]
fn default_type_bound_rejects_mismatched_default_value() {
    let module = resolve(concat!(
        "type Box<T = String> = (value: T)\n",
        "let boxed: Box = (value: 42)\n",
    ));
    TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("defaulted `Box` should require a `String` value, not `I32`");
}

#[test]
fn default_type_bound_may_reference_an_earlier_parameter() {
    let module = type_check(concat!(
        "type Pair<A, B = A> = (A, B)\n",
        "let same: Pair<I32> = (1, 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("chained default should compile");
}

#[test]
fn default_type_bound_referencing_an_earlier_parameter_rejects_mismatch() {
    let module = resolve(concat!(
        "type Pair<A, B = A> = (A, B)\n",
        "let bad: Pair<I32> = (1, \"x\")\n",
    ));
    TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("`B` should default to `I32`, not accept a `String`");
}

#[test]
fn default_type_bound_is_checked_against_subtype_bound() {
    let module = resolve(concat!(
        "type Constrained<T = I32 where T <: String> = T\n",
        "def bad: Constrained\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("`I32` default should not satisfy `T <: String`");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "subtype bound is not satisfied: `I32` is not a subtype of `String`"
    }));
}

#[test]
fn default_type_bound_does_not_fire_when_a_later_parameter_lacks_one() {
    let module = resolve(concat!(
        "type Weird<A = I32, B> = (A, B)\n",
        "let bad: Weird = (1, 2)\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics(concat!(
            "`Weird`'s `B` parameter has no default, so the defaulted `A` ",
            "cannot fill in for a fully bare `Weird` either"
        ));
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "type constructor `Weird` expects 2 type arguments, but 0 were supplied"
    }));
}

#[test]
fn trait_default_type_bound_fills_missing_implementation_and_bound_arguments() {
    let module = type_check(concat!(
        "trait Converts From (To = String) { convert: From -> To }\n",
        "impl Converts I32 { convert = value => to_string value }\n",
        "def show: <T where Converts T> T -> String = <T where Converts T> value => convert value\n",
        "let text: String = show 42\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("defaulted trait argument should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn inline_default_type_bound_introduces_and_defaults_in_one_clause() {
    let module = type_check(concat!(
        "type Pair<A, B = A> = (A, B)\n",
        "let same: Pair<I32> = (1, 2)\n",
        "let overridden: Pair<I32, String> = (1, \"x\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("inline default should compile just like the two-clause form");
}

#[test]
fn inline_default_type_bound_combines_with_a_trailing_subtype_bound() {
    let module = type_check(concat!(
        "type Ident2<Spelling = String where Spelling <: String> = Spelling\n",
        "let literal: Ident2 = \"answer\"\n",
        "let widened: Ident2<String> = \"answer\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("inline default combined with a trailing subtype bound should compile");
}

#[test]
fn inline_default_type_bound_for_trait_parameter_fills_missing_argument() {
    let module = type_check(concat!(
        "trait Converts From (To = String) { convert: From -> To }\n",
        "impl Converts I32 { convert = value => to_string value }\n",
        "def show: <T where Converts T> T -> String = <T where Converts T> value => convert value\n",
        "let text: String = show 42\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("inline trait default should compile just like the two-clause form");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn rejects_duplicate_default_type_bound_for_the_same_parameter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("type Bad<T = I32, T = String> = T\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("`T` cannot have two conflicting defaults");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "duplicate default type bound for compile-time parameter `T`"
    }));
}

#[test]
fn evaluates_pure_compile_time_control_flow_and_arithmetic() {
    let module = type_check(concat!(
        "macro computed = unused => match 1 + 1 == 2 {\n",
        "    True() => parse_quote { 42 },\n",
        "    False() => parse_quote { 0 },\n",
        "}\n",
        "let result: I32 = computed ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("pure compile-time arithmetic should select syntax");
}

#[test]
fn composes_nested_macros_and_applies_excess_arguments() {
    let module = type_check(concat!(
        "macro inner = value => parse_quote { $value }\n",
        "macro outer = value => parse_quote { inner $value }\n",
        "macro identity = value => parse_quote { $value }\n",
        "def increment: I32 -> I32 = value => value + 1\n",
        "let nested: I32 = outer 41\n",
        "let applied: I32 = identity increment 41\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nested and excess-argument macro calls should compile");
}

#[test]
fn applies_excess_arguments_to_a_bare_quote_invoked_as_a_top_level_item() {
    // `identity` returns opaque `Syntax`, so its expansion is reparsed as an
    // item, not an expression; that reparse must still apply the excess
    // `"..."` argument left after `println`, rather than silently dropping
    // it and only ever loading the `println` closure without calling it.
    let module = type_check(concat!(
        "use std.io.println\n",
        "macro identity = value: Expr => quote { $value }\n",
        "identity println \"excess arguments follow the expansion\"\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("excess arguments after a top-level `quote` expansion should still apply");
    assert!(
        llvm.contains("call {} %closure.code"),
        "expected the expanded `println` call to survive expansion:\n{llvm}",
    );
}

#[test]
fn diagnoses_incomplete_and_non_syntax_macros() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let incomplete = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro pair = (left; right) => parse_quote { ($left, $right) }\npair 1\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(incomplete)
        .expect_err_diagnostics("incomplete macro call should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("requires 2 arguments"))
    );

    let invalid = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("macro invalid = value => 42\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(invalid)
        .expect_err_diagnostics("non-Syntax macro result should fail without invocation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "macro `invalid` must return `Syntax`")
    );
}

#[test]
fn panic_called_during_macro_expansion_reports_a_clean_diagnostic() {
    // `panic`'s real body calls extern `c` functions (`printf`, `exit`),
    // which have no compile-time meaning; the macro interpreter used to
    // try to evaluate that body anyway and fail deep inside it with a
    // confusing `` compile-time name `printf` is not available `` error,
    // plus a redundant `while expanding macro` trailer. `panic` is now
    // recognized by identity and short-circuited into a single clean
    // diagnostic before its body is ever interpreted.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let source = concat!(
        "use std.process.panic\n",
        "macro always_fail = e: Expr => panic \"internal error\"\n",
        "always_fail 42\n",
    );
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("a compile-time panic should fail resolution");
    assert_eq!(
        diagnostics.len(),
        1,
        "expected exactly one diagnostic, got {diagnostics:?}",
    );
    assert_eq!(
        diagnostics[0].message,
        "macro `always_fail` panicked: internal error",
    );
    // The span's start includes the newline that ends the previous line
    // (an existing quirk of how top-level statement spans are computed,
    // unrelated to this fix); what matters here is that it lands on the
    // `always_fail 42` call-site line rather than inside the macro's own
    // `panic "internal error"` definition.
    let call_site = source
        .rfind("\nalways_fail 42")
        .expect("call site should be present in the source");
    assert_eq!(
        diagnostics[0].span.to_range().start,
        call_site,
        "the diagnostic should point at the macro call site, not at `panic` inside \
         the macro's own definition",
    );
}

#[test]
fn rejects_runtime_syntax_values() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let generated: Syntax = 1\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("Syntax should not reach runtime checking");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`Syntax` values are compile-time-only")
    );
}

#[test]
fn diagnoses_recursive_macro_expansion() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(
                "macro recursive = value => parse_quote { recursive $value }\nrecursive 1\n",
            ),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("recursive expansion should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "recursive macro expansion of `recursive`")
    );
}

#[test]
fn rejects_repeated_splices_with_a_specific_diagnostic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("macro many = values => parse_quote { $values... }\nmany 1\n"),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("repeated splice should be reserved");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "repeated splices are not supported yet")
    );
}

#[test]
fn type_checks_function_declarations_without_values() {
    type_check("def add: (x: I32, y: I32) -> I32\n");
}

#[test]
fn reports_unknown_names_without_panicking() {
    let syntax = parse("missing\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("unknown name should fail resolution");

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "unknown name `missing`");
}

#[test]
fn reports_duplicate_definitions() {
    let syntax = parse("def value = 1\ndef value = 2\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("duplicate name should fail resolution");

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "duplicate definition of `value`");
}

#[test]
fn a_later_let_shadows_an_earlier_one_in_the_same_scope() {
    let syntax = parse("let value = 1\nlet value = 2\n").expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("a later `let` should be allowed to shadow an earlier one");

    let syntax = parse("let value = 1\nlet mut value = 2\n").expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("`let mut` should be allowed to shadow an earlier `let`");

    let syntax = parse("let mut value = 1\nlet value = 2\n").expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("`let` should be allowed to shadow an earlier `let mut`");

    let syntax = parse("let a = 1\nlet (a, b) = (2, 3)\n").expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("a destructuring `let` should be allowed to shadow an earlier binding");
}

#[test]
fn rejects_a_name_bound_more_than_once_in_the_same_pattern() {
    let syntax = parse("let (a, a) = (1, 2)\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("a pattern must not bind the same name twice");

    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`a` is bound more than once in the same pattern")
    }));
}

#[test]
fn rejects_a_def_colliding_with_a_let_of_the_same_name_regardless_of_order() {
    let syntax = parse("let x = 1\ndef x = () => x\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("a `let` must not be shadowed by a `def`");
    assert_eq!(diagnostics[0].message, "duplicate definition of `x`");

    let syntax = parse("def x = () => x\nlet x = 1\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("a `def` must not be shadowed by a `let`");
    assert_eq!(diagnostics[0].message, "duplicate definition of `x`");
}

#[test]
fn rejects_two_pub_bindings_of_the_same_name_but_allows_pub_and_private_to_shadow() {
    let syntax = parse("pub let value = 1\npub let value = 2\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("two `pub` bindings of the same name must stay an error");
    assert_eq!(diagnostics[0].message, "duplicate definition of `value`");

    let syntax = parse("pub let value = 1\nlet value = 2\n").expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("a private binding should be allowed to shadow a `pub` one");

    let syntax = parse("let value = 1\npub let value = 2\n").expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("a `pub` binding should be allowed to shadow a private one");
}

#[test]
fn a_shadowing_let_produces_the_later_value_at_codegen() {
    let module = type_check("let value = 1\nlet value = value + 1\nvalue\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a shadowing `let` should compile");
    assert!(llvm.contains("add"));
}

#[test]
fn generates_a_named_function_after_predeclaring_it() {
    let module = type_check("let first = (a: I32, b: I32) => a\nfirst (1, 2)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("module should compile");

    assert!(llvm.contains("define i32 @first(ptr %0, i32 %a, i32 %b)"));
    assert!(llvm.contains("closure.call"));
}

#[test]
fn predeclares_functions_for_recursion() {
    let module = type_check("def recurse: (n: I32) -> I32 = (n: I32) => recurse (n)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("recursive function should compile");

    assert!(llvm.contains("binding.uninitialized"));
    assert!(llvm.contains("closure.call"));
}

#[test]
fn preserves_the_environment_for_recursive_closures() {
    let module = type_check(concat!(
        "def outer = value: I32 => {\n",
        "  def recurse: I32 -> I32 = n => {\n",
        "    let captured = value\n",
        "    recurse n\n",
        "  }\n",
        "  recurse 1\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("recursive closure should compile");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("load { i32, ptr }"));
}

#[test]
fn lowers_captured_locals_into_closure_environments() {
    let module = type_check(concat!(
        "let outer = (value: I32) => {\n",
        "  let inner = () => value\n",
        "  inner ()\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("captured locals should use closure lowering");

    assert!(llvm.contains("@malloc"));
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("load { i32 }"));
}

#[test]
fn type_checks_and_lowers_managed_shared_values() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "let point: Shared<(x: I32, y: I32)> = Shared (x: 1, y: 2)\n",
        "let copy = point\n",
        "let x: I32 = copy.x\n",
        "let Shared (captured_x, captured_y) = point\n",
        "def sum_shared: (Shared<(x: I32, y: I32)>) -> I32 = value => match value {\n",
        "  Shared (x, y) => x + y,\n",
        "}\n",
        "let nested: Shared<Shared<I32>> = Shared (Shared 9)\n",
        "let empty: Shared<()> = Shared ()\n",
        "let total: I32 = captured_x + captured_y\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("Shared values should generate valid LLVM");
    assert!(llvm.contains("call ptr @__staple_gc_alloc"));
    assert!(llvm.contains("shared.payload"));
    assert!(llvm.contains("@__staple_gc_collect"));
    assert!(llvm.contains("call void @__staple_gc_register_root"));
}

#[test]
fn compares_shared_values_through_the_standard_library_eq_implementation() {
    let source = concat!(
        "use std.shared.(Shared)\n",
        "use std.buffer.Buffer\n",
        "use std.cmp.Eq\n",
        "def shared_equal: <T where Eq T> (Shared<T>, Shared<T>) -> Bool = <T where Eq T> (left, right) => left == right\n",
        "let a: Shared<I32> = Shared 1\n",
        "let b: Shared<I32> = Shared 1\n",
        "let c: Shared<I32> = Shared 2\n",
        "let same: Bool = a == b\n",
        "let different: Bool = a != c\n",
        "let generic: Bool = shared_equal (a, b)\n",
        "let nested: Bool = Shared (Shared 3) == Shared (Shared 3)\n",
        "type Bag wrap Buffer<I32>\n",
        "impl Eq Bag {\n",
        "  eq = (left; right) => Buffer.capacity left.* == Buffer.capacity right.*\n",
        "}\n",
        "let bags: Bool = Shared (Bag (Buffer.with_capacity 2)) == Shared (Bag (Buffer.with_capacity 2))\n",
        "let bags_differ: Bool = Shared (Bag (Buffer.with_capacity 2)) != Shared (Bag (Buffer.with_capacity 3))\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("comparing Shared values should use the standard-library Eq implementation");
}

#[test]
fn rejects_shared_equality_when_the_payload_is_not_eq() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.shared.(Shared)\n",
            "use std.buffer.Buffer\n",
            "let left: Shared<Buffer<I32>> = Shared (Buffer.with_capacity 1)\n",
            "let right: Shared<Buffer<I32>> = Shared (Buffer.with_capacity 1)\n",
            "let same: Bool = left == right\n",
        )))
        .expect_err_diagnostics("a payload without Eq must not make its Shared comparable");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("no trait implementation or matching bound is available")
    }));
}

#[test]
fn preserves_literal_nominal_shared_container_semantics() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "type SharedPoint wrap Shared<(x: I32, y: I32)>\n",
        "let point: SharedPoint = SharedPoint (Shared (x: 3, y: 4))\n",
        "let x: I32 = point.x\n",
        "let SharedPoint (Shared (captured_x, captured_y)) = point\n",
        "let y: I32 = captured_y\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nominal Shared containers should compile");
}

#[test]
fn rejects_incomplete_and_overapplied_shared_types() {
    let diagnostics = TypeChecker::new()
        .check(resolve("use std.shared.(Shared)\ndef value: Shared\n"))
        .expect_err_diagnostics("Shared requires a payload type");
    assert!(!diagnostics.is_empty());

    let diagnostics = TypeChecker::new()
        .check(resolve("use std.shared.(Shared)\ndef value: Shared<I32, I32>\n"))
        .expect_err_diagnostics("Shared accepts exactly one payload type");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("does not accept compile-time arguments")
    }));
}

#[test]
fn type_checks_and_generates_curried_functions() {
    let module = type_check(concat!(
        "def inferred = a: I32 => b: I32 => a\n",
        "def annotated: I32 -> I32 -> I32 = a => b => a\n",
        "def add_one = annotated 1\n",
        "inferred 1 2\n",
        "add_one 2\n",
    ));

    for name in ["inferred", "annotated"] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .expect("curried function should resolve");
        assert_eq!(
            module.type_of_function(function.id).expect("checked type"),
            &staple_compiler::CheckedFunctionType {
                default: None,
                parameter: Box::new(CheckedType::I32),
                mutations: Vec::new(),
                moves: Vec::new(),
                effects: staple_compiler::CheckedEffectSet::default(),
                result: Box::new(CheckedType::Function(
                    staple_compiler::CheckedFunctionType {
                        default: None,
                        parameter: Box::new(CheckedType::I32),
                        mutations: Vec::new(),
                        moves: Vec::new(),
                        effects: staple_compiler::CheckedEffectSet::default(),
                        result: Box::new(CheckedType::I32),
                    }
                )),
            },
        );
    }

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("curried functions should compile");
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("@malloc"));
}

#[test]
fn type_checks_and_generates_non_curried_juxtaposed_functions() {
    let source = concat!(
        "let pair_add: (x: I32; y: I32) -> I32 = (x: I32; y: I32) => x + y\n",
        "let result: I32 = pair_add 1 2\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("juxtaposed functions should generate LLVM");
}

#[test]
fn rejects_incomplete_juxtaposed_calls() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let pair_add: (x: I32; y: I32) -> I32 = (x; y) => x + y\n",
            "let partial = pair_add 1\n",
        )))
        .expect_err_diagnostics("juxtaposed calls cannot be partial");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("incomplete juxtaposed function call")
    }));
}

#[test]
fn completes_a_juxtaposed_call_before_calling_its_result() {
    let source = concat!(
        "let choose: (x: I32; y: I32) -> I32 -> I32 = (x; y) => z => x + y + z\n",
        "let result: I32 = choose 1 2 3\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("remaining arguments should call the completed result");
}

#[test]
fn keeps_product_and_juxtaposed_function_types_distinct() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "let juxtaposed: (x: I32; y: I32) -> I32 = (x; y) => x + y\n",
            "let product: (I32, I32) -> I32 = juxtaposed\n",
        )))
        .expect_err_diagnostics("function parameter styles must remain distinct");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `(I32, I32) -> I32`"))
    );
}

#[test]
fn supports_patterns_in_juxtaposed_parameter_slots() {
    let source = concat!(
        "let combine: ((I32, I32); I32) -> I32 = ((left, right); _) => left + right\n",
        "let result: I32 = combine (1, 2) 3\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("juxtaposed slots should support ordinary parameter patterns");
}

#[test]
fn lowers_mutation_markers_on_juxtaposed_parameter_slots() {
    let source = concat!(
        "let replace: (mut target: I32; value: I32) -> () = (mut target; value) => { target = value }\n",
        "let mut number = 1\n",
        "replace number 9\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("juxtaposed mutation slots should use the flattened parameter ABI");
}

#[test]
fn rejects_duplicate_functions_at_the_same_arity() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new().with_standard_library_root(root.join("stdlib")).load_source("def duplicate: I32 -> I32 = value => value\ndef duplicate: String -> String = value => value\n", root).expect("source loads");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("duplicate names must be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("duplicate definition of `duplicate`")),
        "{diagnostics:?}"
    );
}

#[test]
fn contextually_types_product_parameters() {
    type_check("def first: (I32, I32) -> I32 = (a, b) => a\nfirst (1, 2)\n");
}

#[test]
fn rejects_untyped_parameters_without_a_function_annotation() {
    let module = resolve("def identity = value => value\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("an untyped parameter needs context");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("could not fully infer"))
    );
}

#[test]
fn supports_typed_wildcard_parameters() {
    type_check("def demo = _: String => ()\ndemo \"unused\"\n");
}

#[test]
fn lowers_transitive_captures_across_curried_layers() {
    let module = type_check(concat!(
        "def sum = a: I32 => b: I32 => c: I32 => (a, b, c)\n",
        "def add_one = sum 1\n",
        "def add_one_two = add_one 2\n",
        "add_one_two 3\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("transitive captures should compile");
    assert!(llvm.matches("@malloc").count() >= 3);
    assert!(llvm.contains("load { i32 }"));
    assert!(llvm.contains("load { i32, i32 }"));
}

#[test]
fn adapts_non_variadic_externs_used_as_function_values() {
    let module = type_check(concat!(
        "use std.cinterop.*\nuse std.ptr.*\n",
        "extern \"c\" { puts: (RawPtr<CChar>) -> I32 }\n",
        "def apply: ((RawPtr<CChar>) -> I32, RawPtr<CChar>) -> I32 = (f, value) => f value\n",
        "apply (puts, c_string \"hello\")\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("external function adapter should compile");
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::ExternAdapter);
    assert!(
        lowered
            .planned_artifact_names(PlannedArtifactFamily::ExternAdapter)
            .iter()
            .any(|name| llvm.contains(&format!("define internal i32 @{name}(")))
    );
    assert!(llvm.contains("call i32 @puts"));
}

#[test]
fn compiles_builtin_arithmetic_via_trait_dispatch() {
    let module = type_check("let answer: I32 = 1 + 2\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("builtin `+` should compile via ordinary trait dispatch");
    // "operator.2b" is the mangled symbol a binding literally named `+` would
    // get; since `+` is fixed grammar now, not a binding, it must not appear.
    assert!(!llvm.contains("@operator.2b"));
    assert!(llvm.contains("add"));
}

#[test]
fn compiles_arithmetic_negation_via_neg_trait() {
    let module = type_check(concat!(
        "let literal: I32 = -5\n",
        "let x: I64 = 3\n",
        "let negated: I64 = -x\n",
        "let stacked: I32 = - -literal\n",
        "let precedence: I32 = -literal * 2\n",
        "let float: F64 = -1.5\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("prefix `-` should compile via `Neg` trait dispatch");
}

/// A one-element product literal `(e)` or `(name: e)` is `e` itself, even when
/// `e` is a product: it checks against an expected product as a whole and
/// places `e`'s slots as call arguments, rather than as one slot of its own.
#[test]
fn compiles_singleton_product_literals_wrapping_products() {
    let module = type_check(concat!(
        "use std.io.println\n",
        "type Box<T> wrap (value: T)\n",
        "type Defaulted wrap (a: I32, b: I32 = 5)\n",
        "def sum: (I32, I32) -> I32 = (a, b) => a + b\n",
        "let s = (3, 4)\n",
        "let named: Box<(I32, I32)> = Box (value: (1, 2))\n",
        "let unnamed: Box<(I32, I32)> = Box ((1, 2))\n",
        "let symbol: Box<(I32, I32)> = Box (value: s)\n",
        "let pair: (I32, I32) = (value: (1, 2))\n",
        "let defaulted = Defaulted (a: 1)\n",
        "println \"${named.value.0} ${unnamed.value.1} ${symbol.value.0} ${pair.1}\"\n",
        "println \"${sum (value: (1, 2))} ${sum ((1, 2))} ${defaulted.b}\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("singleton product literals around products should compile");
}

#[test]
fn compiles_logical_not_via_not_trait() {
    let module = type_check(concat!(
        "let t: Bool = True\n",
        "let f: Bool = False\n",
        "let flipped: Bool = !t\n",
        "let doubled: Bool = !!f\n",
        "let combined: Bool = !(1 == 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("prefix `!` should compile via `Not` trait dispatch");
}

#[test]
fn prefix_operators_dispatch_to_user_defined_impls() {
    let module = type_check(concat!(
        "use std.ops.(Neg, Not)\n",
        "type Vec2 wrap (x: I32, y: I32)\n",
        "impl Neg Vec2 { negate = (Vec2 (x, y)) => Vec2 (x: 0 - x, y: 0 - y) }\n",
        "type Flag wrap (raised: Bool)\n",
        "impl Not Flag { not = (Flag (raised)) => Flag (raised: !raised) }\n",
        "let here: Vec2 = Vec2 (x: 3, y: 4)\n",
        "let away: Vec2 = -here\n",
        "let up: Flag = Flag (raised: True)\n",
        "let down: Flag = !up\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("user `Neg`/`Not` implementations should back the prefix operators");
}

#[test]
fn folds_const_prefix_negation_at_compile_time() {
    let module = type_check(concat!(
        "const negative: I32 = -7\n",
        "const restored: I32 = - -7\n",
        "const scaled: I32 = -7 * 3\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("const prefix negation should fold and compile");
}

#[test]
fn rejects_arithmetic_negation_of_unsigned_integers() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let bad: U32 = -(U32 :: 1)\n"))
        .expect_err_diagnostics("unsigned integers have no `Neg` implementation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation")),
        "expected a missing-implementation diagnostic, found {diagnostics:#?}",
    );
}

#[test]
fn decodes_source_string_literals_before_llvm_generation() {
    let source = concat!(
        "use std.cinterop.*\nuse std.ptr.*\n",
        "extern \"c\" { puts: (RawPtr<CChar>) -> I32 }\n",
        "puts (c_string \"hello\\n\")\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("module should compile");

    assert!(llvm.contains("c\"hello\\0A\\00\""));
    assert!(!llvm.contains("\\22hello"));
}

#[test]
fn type_checks_generic_aliases_and_functions() {
    let module = type_check(concat!(
        "type Pair<A, B> = (A, B)\n",
        "def identity: <T> move T -> T = <T> move x => x\n",
        "let pair: Pair<String, I32> = (\"answer\", 42)\n",
        "let answer: I32 = identity 42\n",
        "let text: String = identity \"hello\"\n",
    ));
    assert!(
        module
            .functions()
            .iter()
            .any(|function| function.name == "identity")
    );
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("generic functions should be monomorphized");
    let names = lowered.planned_instance_names("identity");
    assert!(names.len() >= 2);
    for name in names {
        function_definition(&llvm, &name);
    }
}

#[test]
fn type_checks_static_traits_and_bounded_generic_functions() {
    let module = type_check(concat!(
        "trait Increment T { increment: T -> T }\n",
        "trait Echo T { echo: T -> T }\n",
        "trait Swap T { swap: T -> T }\n",
        "impl Increment I32 { increment = value => value + 1 }\n",
        "impl Echo I32 { echo = value => value }\n",
        "impl Swap (I32, I32) { swap = (left, right) => (right, left) }\n",
        "def increment_twice: <T where Increment T> T -> T = <T where Increment T> value => increment (increment value)\n",
        "def increment_echo: <T where Increment T, Echo T> T -> T = <T where Increment T, Echo T> value => echo (increment value)\n",
        "let direct: I32 = Increment.increment 40\n",
        "let answer: I32 = increment_twice direct\n",
        "let bounded: I32 = increment_echo answer\n",
        "let first_class: I32 -> I32 = Increment.increment\n",
        "let other: I32 = first_class bounded\n",
        "let swapped: (I32, I32) = Swap.swap (1, 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("static trait calls should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn provides_to_string_for_prelude_scalar_types() {
    let module = type_check(concat!(
        "def render: <T where ToString T> T -> String = <T where ToString T> value => to_string value\n",
        "let a = render (I8 :: 1)\nlet b = render (I16 :: 1)\n",
        "let c = render (I32 :: 1)\nlet d = render (I64 :: 1)\n",
        "let e = render (U8 :: 1)\nlet f = render (U16 :: 1)\n",
        "let g = render (U32 :: 1)\nlet h = render (U64 :: 1)\n",
        "let i = render (ISize :: 1)\nlet j = render (USize :: 1)\n",
        "let k = render (F32 :: 1.5)\nlet l = render (F64 :: 1.5)\n",
        "let boolean: Bool = True\nlet m = render boolean\n",
        "let string: String = \"text\"\nlet n = render string\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("ToString implementations should generate LLVM");
}

#[test]
fn provides_formatter_display_debug_and_structural_product_debug() {
    let module = type_check(concat!(
        "use std.fmt.Formatter\n",
        "type Point wrap (x: I32, y: I32)\n",
        "impl Debug Point {\n",
        "  fmt = (Point (x, y), mut formatter) => {\n",
        "    Formatter.write formatter \"Point \"\n",
        "    Debug.fmt ((x: x, y: y), formatter)\n",
        "  }\n",
        "}\n",
        "def exercise: () -> () = () => {\n",
        "let mut formatter = Formatter.new ()\n",
        "Formatter.write formatter \"left\"\n",
        "Formatter.write formatter \" + right\"\n",
        "let written: String = Formatter.finish formatter\n",
        "let displayed: String = Formatter.display 42\n",
        "let escaped: String = Formatter.debug \"hello\\n\\\"world\"\n",
        "let product: (I32, name: String, Bool) = (1, name: \"two\", True)\n",
        "let product_debug: String = Formatter.debug product\n",
        "let empty_debug: String = Formatter.debug ()\n",
        "let point_debug: String = Formatter.debug (Point (x: 3, y: 4))\n",
        "()\n}\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("formatting protocols should generate LLVM");
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::StructuralDebug);
    assert!(llvm.contains("formatter.write"));
}

#[test]
fn provides_structural_debug_for_sum_types() {
    let module = type_check(concat!(
        "use std.fmt.Formatter\n",
        "let integer: I32 | String = 42\n",
        "let string: I32 | String = \"text\"\n",
        "let integer_debug: String = Formatter.debug integer\n",
        "let string_debug: String = Formatter.debug string\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("sum Debug should generate LLVM");
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::StructuralDebug);
    assert!(llvm.contains("debug.sum.fmt"));
}

#[test]
fn derives_debug_for_nominal_representations() {
    let module = type_check(concat!(
        "use std.fmt.*\n",
        "@derive_debug\ntype Point wrap (x: I32, y: I32)\n",
        "@derive_debug\ntype Choice wrap I32 | String\n",
        "@derive_debug\ntype Box<T> wrap T\n",
        "let point_debug: String = Formatter.debug (Point (x: 3, y: 4))\n",
        "let choice_debug: String = Formatter.debug (Choice (I32 | String :: 42))\n",
        "let box_debug: String = Formatter.debug (Box 7)\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("derived Debug implementations should generate LLVM");
    assert!(llvm.contains("formatter.write"));
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::StructuralDebug);
}

#[test]
fn derives_debug_for_bodyless_singleton_types() {
    let module = type_check(concat!(
        "use std.fmt.*\n",
        "@derive_debug\ntype Enabled\n",
        "@derive_debug\npub type Disabled\n",
        "let enabled_debug: String = Formatter.debug Enabled\n",
        "let disabled_debug: String = Formatter.debug Disabled\n",
        "let choice_debug: String = Formatter.debug (Enabled | Disabled :: Enabled)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("derived singleton Debug implementations should generate LLVM");
    // Each singleton's name is emitted as a string literal for its Debug impl,
    // and the sum reuses those per-variant impls through structural sum Debug.
    assert!(llvm.contains("c\"Enabled\\00\""));
    assert!(llvm.contains("c\"Disabled\\00\""));
    assert!(llvm.contains("debug.sum.fmt"));
}

#[test]
fn quoted_type_declarations_from_one_template_get_distinct_identity() {
    // `typegroup` generates every variant `type` from a single `parse_quote`
    // template; applying `@derive_debug` to more than one variant used to emit
    // two `impl Debug` whose targets shared the template identifier node, which
    // the resolver collapsed into one and rejected as a duplicate.
    let module = type_check(concat!(
        "use std.fmt.*\n",
        "typegroup Switch {\n",
        "    @derive_debug\n",
        "    Enabled,\n",
        "    @derive_debug\n",
        "    Disabled,\n",
        "}\n",
        "use Switch.*\n",
        "let enabled_debug: String = Formatter.debug Enabled\n",
        "let disabled_debug: String = Formatter.debug Disabled\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("distinct per-variant Debug implementations should generate LLVM");
    assert!(llvm.contains("c\"Enabled\\00\""));
    assert!(llvm.contains("c\"Disabled\\00\""));
}

#[test]
fn exposes_type_declarations_as_structured_items() {
    let module = type_check(concat!(
        "macro @inspect_item: Item -> Item = item => match item {\n",
        "  TypeDeclarationItem (WrapperDeclaration(), Ident name, spelling, declared_type, parameters, Some representation) => item,\n",
        "  UnstructuredItem() => item,\n",
        "  _ => panic \"inspect_item item is unsupported\",\n",
        "}\n",
        "@inspect_item\ntype Pair<T> wrap (T, T)\n",
        "@inspect_item\ndef answer = () => 42\n",
        "let pair = Pair (1, 2)\nlet result = answer ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("structured and fallback item views should round-trip");
}

#[test]
fn type_checks_and_generates_string_templates() {
    let module = type_check(concat!(
        "use std.fmt.Formatter\n",
        "type Label wrap String\n",
        "impl Display Label {\n",
        "  fmt = (Label value, mut formatter) => Formatter.write formatter value\n",
        "}\n",
        "def render: <T where Display T> move T -> String = <T where Display T> move value => \"value=$value\"\n",
        "let name: String = \"world\"\n",
        "let answer: I32 = 42\n",
        "let product = (answer, name)\n",
        "let message: String = \"hello $name: ${answer}; ${product:?}; \\$5\"\n",
        "let generic: String = render answer\n",
        "let nominal: String = \"label=${Label \"tag\"}\"\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("string templates should generate LLVM");
    assert!(llvm.contains("template.formatter"));
    assert!(llvm.contains("template.fmt"));
    assert!(llvm.contains("template.finish"));
}

#[test]
fn string_templates_require_the_selected_formatting_trait() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.fmt.Formatter\n",
            "type Secret wrap I32\n",
            "let secret = Secret 1\n",
            "let message = \"$secret\"\n",
        )))
        .expect_err_diagnostics("Display is required for ordinary interpolation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("trait bound is not satisfied") })
    );
}

#[test]
fn bounds_on_traits_with_associated_types_are_checked_at_calls() {
    // The associated type rides along as a projection placeholder the call
    // does not determine; it used to defer the bound, so any argument passed.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Shaped T {\n",
            "    type Shape\n",
            "    shape: T -> I32\n",
            "}\n",
            "type Square wrap I32\n",
            "type Plain wrap I32\n",
            "impl Shaped Square { type Shape = I32; shape = value => 4 }\n",
            "def sides = <T where Shaped T> value: T => I32 :: 1\n",
            "let square = sides (Square 1)\n",
            "let plain = sides (Plain 1)\n",
        )))
        .expect_err_diagnostics("`Plain` does not implement `Shaped`");
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        ["trait bound is not satisfied for `Plain`"],
        "{messages:?}"
    );
}

#[test]
fn structural_debug_requires_debug_elements_and_does_not_expose_nominal_representations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.fmt.Formatter\n",
            "type Secret wrap I32\n",
            "let secret = Secret 1\n",
            "let product = (secret,)\n",
            "let text = Formatter.debug product\n",
        )))
        .expect_err_diagnostics("a product element without Debug must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("trait bound is not satisfied") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.fmt.Formatter\n",
            "type Secret wrap I32\n",
            "let secret = Secret 1\n",
            "let text = Formatter.debug secret\n",
        )))
        .expect_err_diagnostics(
            "a nominal type must not inherit its representation's Debug implementation",
        );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("trait bound is not satisfied") })
    );
}

#[test]
fn uses_generic_default_trait_members_and_concrete_overrides() {
    let module = type_check(concat!(
        "trait Increment T {\n",
        "  increment: T -> T\n",
        "  twice: T -> T = value => increment (increment value)\n",
        "}\n",
        "impl Increment I32 { increment = value => value + 1 }\n",
        "let direct: I32 = Increment.twice 40\n",
        "let first_class: I32 -> I32 = Increment.twice\n",
        "let answer: I32 = first_class direct\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("default trait members should specialize and compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn default_trait_members_use_prerequisites_multiple_arguments_and_macros() {
    let module = type_check(concat!(
        "use std.cmp.Eq\n",
        "trait Same T where Copy T, Eq T { same: (T, T) -> Bool = (left, right) => Eq.eq (left; right) }\n",
        "trait Select Value { select: (Bool, move Value, move Value) -> Value = (condition, move left, move right) => when { condition => left, else => right } }\n",
        "trait First (Left, Right) { first: (move Left, Right) -> Left = (move left, right) => left }\n",
        "impl Same I32 {}\n",
        "impl Select I32 {}\n",
        "impl First (I32, String) {}\n",
        "let condition: Bool = Same.same (42, 42)\n",
        "let selected: I32 = Select.select (condition, 1, 2)\n",
        "let first: I32 = First.first (selected, \"ignored\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("prerequisite and macro-using defaults should compile");
}

#[test]
fn explicit_trait_members_override_defaults() {
    let module = type_check(concat!(
        "trait Identity T { identity: move T -> T = move value => value }\n",
        "impl Identity I32 { identity = move value => value + 1 }\n",
        "let answer: I32 = Identity.identity 41\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("explicit overrides should compile");
}

#[test]
fn specializes_recursive_default_trait_members() {
    let module = type_check(concat!(
        "trait Recursive T { recurse: T -> T = value => Recursive.recurse value }\n",
        "impl Recursive I32 {}\n",
        "let result: I32 = Recursive.recurse 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("recursive defaults should reuse their specialization");
}

#[test]
fn rejects_invalid_default_trait_member_bodies() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "trait Invalid T { identity: T -> T = value => \"wrong\" }\n",
        ))
        .expect_err_diagnostics("default bodies must match their member type");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("expected `T`, found `String`") })
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("trait Invalid T { identity: T -> T = 42 }\n", root)
        .expect("invalid default source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("default bodies must be function values");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("default trait member implementations must be function values")
    }));
}

#[test]
fn type_checks_product_and_curried_multi_parameter_traits() {
    let module = type_check(concat!(
        "trait Merge Left Right Output { merge: (Left, Right) -> Output }\n",
        "trait Convert (From, To) { convert: From -> To }\n",
        "impl Merge I32 I32 I32 { merge = (left, right) => left + right }\n",
        "impl Convert (I32, String) { convert = value => \"converted\" }\n",
        "def combine: <L, R, O where Merge L R O> (L, R) -> O = <L, R, O where Merge L R O> pair => Merge.merge pair\n",
        "let total: I32 = combine (20, 22)\n",
        "let converted: String = Convert.convert total\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("multi-parameter trait calls should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn infers_trait_associated_type_arguments() {
    let module = type_check(concat!(
        "trait Iterator Iter { type Item; next: Iter -> Item }\n",
        "impl Iterator I32 { type Item = String; next = value => \"next\" }\n",
        "trait AddTo Left Right { type Output; add_to: Left -> Right -> Output }\n",
        "impl AddTo I32 I32 { type Output = I32; add_to = left => right => left + right }\n",
        "trait Chain A { type B; type C; chained: A -> (B, C) }\n",
        "impl Chain I32 { type B = String; type C = U8; chained = value => (\"chain\", 7) }\n",
        "trait ConvertPair From { type To; convert_pair: From -> To }\n",
        "impl ConvertPair I32 { type To = String; convert_pair = value => \"pair\" }\n",
        "def requires_iterator: <T where Iterator T> T -> () = <T where Iterator T> value => ()\n",
        "def requires_add: <T where AddTo T T> move T -> T = <T where AddTo T T> move value => value\n",
        "def requires_pair: <T where ConvertPair T> T -> () = <T where ConvertPair T> value => ()\n",
        "trait UsesIterator Iter where Iterator Iter { use_iterator: Iter -> Iter }\n",
        "impl UsesIterator I32 { use_iterator = value => value }\n",
        "let next_value = Iterator.next 1\n",
        "let next_string: String = next_value\n",
        "let sum: I32 = AddTo.add_to 20 22\n",
        "let chained = Chain.chained 1\n",
        "let _: () = requires_iterator 1\n",
        "let _: I32 = requires_add 1\n",
        "let _: () = requires_pair 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("associated type dispatch should compile");
}

#[test]
fn rejects_invalid_associated_type_uses_and_conflicting_impls() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert From { type To; convert: From -> To }\n",
            "impl Convert I32 { type To = String; convert = value => \"one\" }\n",
            "impl Convert I32 { type To = I32; convert = value => value }\n",
        )))
        .expect_err_diagnostics("associated types must make implementations coherent");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("conflicting trait implementations bind different associated types")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Iterator Iter { type Item; next: Iter -> Item }\n",
            "def invalid: <Iter, Item where Iterator Iter Item> Iter -> Iter = <Iter, Item where Iterator Iter Item> value => value\n",
        )))
        .expect_err_diagnostics("bounds cannot name associated types positionally");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expects 1 compile-time argument, found 2")
    }));
}

#[test]
fn infers_unannotated_lambdas_against_normalized_iterator_projections() {
    // Regression test: a juxtaposed pipe stage fills the iterator slot first,
    // but the unannotated lambda in the next slot used to be checked against
    // the still-projected `Iterator.Item Iter` instead of the solved element
    // type, so its body could not resolve the operator's trait arguments.
    let module = type_check(concat!(
        "use std.iterator.(IntoIterator, Iterator, IterStep)\n",
        "type Map<S, Out> wrap (iter: S, step: move S -> IterStep<S, Out>)\n",
        "impl<S, Out> Iterator Map<S, Out> {\n",
        "  type Item = Out\n",
        "  next = move Map (iter, step) => match step iter {\n",
        "    IterStep.Done rest => IterStep.Done (Map.<S, Out> (iter: rest, step: step)),\n",
        "    IterStep.Yield (item, rest) => IterStep.Yield (item, Map.<S, Out> (iter: rest, step: step)),\n",
        "  }\n",
        "}\n",
        "impl<S, Out> IntoIterator Map<S, Out> {\n",
        "  type Iter = Map<S, Out>\n",
        "  into_iterator = move iterator => iterator\n",
        "}\n",
        "def map = <Iter, B where Iterator Iter>\n",
        "  (move iter: Iter; f: move Iterator.Item Iter -> B) => Map<Iter, B> :: {\n",
        "  let step = move it: Iter => IterStep<Iter, B> :: match Iterator.next it {\n",
        "    IterStep.Done rest => IterStep.Done rest,\n",
        "    IterStep.Yield (item, rest) => IterStep.Yield (f item, rest),\n",
        "  }\n",
        "  Map.<Iter, B> (iter: iter, step: step)\n",
        "}\n",
        "let doubled: Map<Range<I32>, I32> = (0 .. 3) |> map (x => x * 2)\n",
    ));
    lower(&module);
}

#[test]
fn resolves_distinct_bounds_on_the_same_trait_in_one_function() {
    // Regression test: every in-scope bound on a trait was treated as a
    // candidate for an inferred obligation, so `Iterator L` and `Iterator R`
    // were merged and failed instead of selecting the one matching the known
    // receiver argument.
    let module = type_check(concat!(
        "use std.iterator.(Iterator, IterStep)\n",
        "def next_both = <L, R where Iterator L, Iterator R>\n",
        "  (move left: L; move right: R) => (Iterator.Item L, Iterator.Item R) :: {\n",
        "  let left_step = Iterator.next left\n",
        "  let right_step = Iterator.next right\n",
        "  (left_step, right_step)\n",
        "}\n",
        "let steps = next_both (0 .. 3) (10 .. 13)\n",
    ));
    lower(&module);
}

#[test]
fn resolves_iterator_projections_named_only_in_a_function_body() {
    // Regression test: a projection first named by a local annotation was
    // created after the function's `where` bound had already been linked, so
    // lowering saw an unbound `Iterator.Item Iter` parameter even though the
    // instance's iterator type was concrete.
    let module = type_check(concat!(
        "use std.iterator.(Iterator, IterStep)\n",
        "type Take<S, Out> wrap (iter: S, left: USize, step: move S -> IterStep<S, Out>)\n",
        "impl<S, Out> Iterator Take<S, Out> {\n",
        "  type Item = Out\n",
        "  next = move Take (iter, left, step) => if (left == 0) {\n",
        "    IterStep.Done (Take.<S, Out> (iter: iter, left: left, step: step))\n",
        "  } else {\n",
        "    match step iter {\n",
        "      IterStep.Done rest => IterStep.Done (Take.<S, Out> (iter: rest, left: left, step: step)),\n",
        "      IterStep.Yield (item, rest) => IterStep.Yield (item, Take.<S, Out> (iter: rest, left: left - 1, step: step)),\n",
        "    }\n",
        "  }\n",
        "}\n",
        "def take = <Iter where Iterator Iter>\n",
        "  (move iter: Iter; count: USize) => Take<Iter, Iterator.Item Iter> :: {\n",
        "  let step = move it: Iter => IterStep<Iter, Iterator.Item Iter> :: Iterator.next it\n",
        "  Take.<Iter, Iterator.Item Iter> (iter: iter, left: count, step: step)\n",
        "}\n",
        "def consume = <Iter where Iterator Iter> (move it: Iter) => () :: {\n",
        "  let taken: Take<Iter, Iterator.Item Iter> = take it 3\n",
        "  ()\n",
        "}\n",
        "let result = consume (0 .. 3)\n",
    ));
    lower(&module);
}

#[test]
fn infers_generic_wrapper_constructor_with_a_function_field() {
    // Regression test: the callee type lookup unwrapped the constructor's
    // `Forall` scheme to its body before collecting the parameters a call may
    // infer, so a function-typed field kept `S` generic and rejected the
    // concrete argument.
    let module = type_check(concat!(
        "type W<S> wrap (state: S, step: S -> S)\n",
        "def inc: I32 -> I32 = x => x + 1\n",
        "let w: W<I32> = W (state: 5, step: inc)\n",
    ));
    lower(&module);
}

#[test]
fn normalizes_the_projection_of_a_second_bound_on_the_same_trait_at_the_call() {
    // Regression test: a slotted call normalizes its result after each slot.
    // After the first slot, inference fell back to the result alone, which
    // the still-open `Iterator.Item R` satisfies trivially, so `R` from the
    // remaining slot was never learned and the item stayed a projection.
    let module = type_check(concat!(
        "use std.iterator.(Iterator, IterStep)\n",
        "def pair = <L, R where Iterator L, Iterator R>\n",
        "  (move left: L; move right: R) => (Iterator.Item L, Iterator.Item R) :: match Iterator.next left {\n",
        "  IterStep.Done _ => panic \"empty\",\n",
        "  IterStep.Yield (a, _) => match Iterator.next right {\n",
        "    IterStep.Done _ => panic \"empty\",\n",
        "    IterStep.Yield (b, _) => (a, b),\n",
        "  },\n",
        "}\n",
        "let (number, byte) = pair (1 .. 3) (String.bytes \"hi\")\n",
        "let checked_number: I32 = number\n",
        "let checked_byte: U8 = byte\n",
        "let (piped_number, piped_byte) = (1 .. 3) |> pair (String.bytes \"hi\")\n",
        "let checked_piped: U8 = piped_byte\n",
    ));
    lower(&module);
}

#[test]
fn solves_effect_parameters_of_slotted_calls_from_later_slots() {
    // Regression test: each call node of `apply 3 (x => ...)` tried to solve
    // `E` from its own argument, and `3` cannot determine it, so the open `E`
    // made the enclosing `def` look generic. The partial application's
    // residual also kept the stale row solved before the lambda's effects
    // were known.
    let module = type_check(concat!(
        "use std.io.println\n",
        "def apply = <effect E> (x: I32; f: I32 ->{E} I32) =>{E} I32 :: f x\n",
        "def pure = () => I32 :: apply 3 (x => x + 1)\n",
        "def piped = () => I32 :: 3 |> apply (x => x + 1)\n",
        "def loud = () => I32 :: apply 3 (x => { println \"called\"; x + 1 })\n",
        "let results = (pure (), piped (), loud ())\n",
    ));
    lower(&module);
    let errors = type_check_errors(
        concat!(
            "use std.io.println\n",
            "def apply = <effect E> (x: I32; f: I32 ->{E} I32) =>{E} I32 :: f x\n",
            "def quiet: () -> I32 = () => apply 3 (x => { println \"called\"; x + 1 })\n",
        ),
        "a pure function cannot call a slotted function with an effectful argument",
    );
    assert!(
        errors.iter().any(|error| error.contains("IO")),
        "{errors:?}"
    );
}

#[test]
fn lowers_wildcards_over_projections_the_signature_never_names() {
    // Regression test: a function's `Iterator Iter` bound left its hidden
    // `Item` slot open when nothing named `Iterator.Item Iter`, so
    // `Iterator.next` produced a placeholder that wildcard patterns, inside
    // the function and inside a closure it builds, carried into lowering.
    let module = type_check(concat!(
        "use std.iterator.(Iterator, IterStep)\n",
        "def count = <Iter where Iterator Iter> (move iter: Iter) => USize :: match Iterator.next iter {\n",
        "  IterStep.Done _ => 0,\n",
        "  IterStep.Yield (_, rest) => 1 + count rest,\n",
        "}\n",
        "def both_present = <L, R where Iterator L, Iterator R> (move left: L; move right: R) => Bool :: {\n",
        "  let present = move (l: L, r: R) => Bool :: match Iterator.next l {\n",
        "    IterStep.Done _ => False,\n",
        "    IterStep.Yield (_, _) => match Iterator.next r {\n",
        "      IterStep.Done _ => False,\n",
        "      IterStep.Yield (_, _) => True,\n",
        "    },\n",
        "  }\n",
        "  present (left, right)\n",
        "}\n",
        "let total = count (0 .. 5)\n",
        "let present = both_present (0 .. 3) (String.bytes \"hi\")\n",
    ));
    lower(&module);
}

#[test]
fn resolves_associated_type_projections() {
    let module = type_check(concat!(
        "trait Container T { type Element; first: T -> Element }\n",
        "impl Container I32 { type Element = String; first = value => \"first\" }\n",
        "let concrete: Container.Element I32 = \"text\"\n",
        "def generic_first: <T where Container T> T -> Container.Element T = <T where Container T> value => Container.first value\n",
        "let from_generic: String = generic_first 1\n",
        "let range_item: Iterator.Item Range<I32> = 5\n",
        "def next_item: <Iter where Iterator Iter> move Iter -> IterStep<Iter, Iterator.Item Iter> = <Iter where Iterator Iter> move iter => Iterator.next iter\n",
        "let step: IterStep<Range<I32>, I32> = next_item (0 .. 3)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("associated type projections should compile");
}

#[test]
fn rejects_invalid_associated_type_projections() {
    for (source, expected) in [
        (
            concat!(
                "trait Container T { type Element; first: T -> Element }\n",
                "def unbounded: <T> T -> Container.Element T = <T> value => value\n",
            ),
            "requires a matching `where` bound",
        ),
        (
            concat!(
                "trait Container T { type Element; first: T -> Element }\n",
                "let missing: Container.Element I32 = 1\n",
            ),
            "no `Container` implementation is available",
        ),
        (
            concat!(
                "trait Lookup T K { type Value; lookup: (T, K) -> Value }\n",
                "let wrong: Lookup.Value I32 = 1\n",
            ),
            "expects 2 compile-time arguments, found 1",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("invalid projection must not type-check");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing `{expected}` in {diagnostics:?}"
        );
    }
}

#[test]
fn rejects_invalid_associated_type_declarations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            concat!(
                "trait Convert From { type To; convert: From -> To }\n",
                "impl Convert I32 { convert = value => \"one\" }\n",
            ),
            "implementation is missing associated type `To`",
        ),
        (
            concat!(
                "trait Convert From { type To; convert: From -> To }\n",
                "impl Convert I32 { type Other = I32; type To = String; convert = value => \"one\" }\n",
            ),
            "trait has no associated type named `Other`",
        ),
        (
            concat!(
                "trait Convert From { type To; convert: From -> To }\n",
                "impl Convert I32 { type To = String; type To = I32; convert = value => \"one\" }\n",
            ),
            "duplicate associated type `To`",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .expect("invalid associated type source should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid associated types must not resolve");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing `{expected}` in {diagnostics:?}"
        );
    }
}

#[test]
fn preserves_applied_types_as_unary_trait_arguments() {
    let module = type_check(concat!(
        "type Box<T> wrap (value: T)\n",
        "trait Echo T { echo: T -> T }\n",
        "impl Echo Box I32 { echo = value => value }\n",
        "def echo_box: <T where Echo Box T> (Box<T>) -> Box<T> = <T where Echo Box T> value => Echo.echo value\n",
        "let boxed: Box<I32> = Box 42\n",
        "let echoed: Box<I32> = echo_box boxed\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("legacy unary applied trait arguments should compile");
}

#[test]
fn enforces_and_propagates_transitive_trait_prerequisites() {
    let module = type_check(concat!(
        "trait Base T { base: T -> T }\n",
        "trait Middle T where Base T { middle: T -> T }\n",
        "trait Derived T where Middle T { derived: T -> T }\n",
        "impl Derived I32 { derived = value => value }\n",
        "impl Middle I32 { middle = value => value }\n",
        "impl Base I32 { base = value => value }\n",
        "def apply: <T where Derived T> T -> T = <T where Derived T> value => Base.base (Middle.middle (Derived.derived value))\n",
        "let answer: I32 = apply 42\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("transitive prerequisite dispatch should compile");
    assert!(llvm.contains("trait.call"));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Base T { base: T -> T }\n",
            "trait Derived T where Base T { derived: T -> T }\n",
            "impl Derived I32 { derived = value => value }\n",
        )))
        .expect_err_diagnostics("implementations must satisfy trait prerequisites");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("trait prerequisite `Base I32` is not satisfied")
    }));
}

#[test]
fn prerequisite_copy_bounds_are_visible_to_ownership_checking() {
    type_check(concat!(
        "trait Duplicate T where Copy T { duplicate: T -> T }\n",
        "impl Duplicate I32 { duplicate = value => value }\n",
        "def pair: <T where Duplicate T> T -> (T, T) = <T where Duplicate T> value => (value, value)\n",
        "let values: (I32, I32) = pair 42\n",
    ));
}

#[test]
fn substitutes_product_parameters_into_multiple_prerequisites() {
    type_check(concat!(
        "use std.cmp.Eq\n",
        "trait BothEqual (Left, Right) where Eq Left, Eq Right { equal: (Left, Left, Right, Right) -> (Bool, Bool) }\n",
        "impl BothEqual (I32, I32) { equal = (left_a, left_b, right_a, right_b) => (Eq.eq (left_a; left_b), Eq.eq (right_a; right_b)) }\n",
        "def compare_both: <Left, Right where Copy Left, Copy Right, BothEqual (Left, Right)> (Left, Left, Right, Right) -> (Bool, Bool) = <Left, Right where Copy Left, Copy Right, BothEqual (Left, Right)> (left_a, left_b, right_a, right_b) => (Eq.eq (left_a; left_b), Eq.eq (right_a; right_b))\n",
        "let result: (Bool, Bool) = compare_both (1, 1, 2, 2)\n",
    ));
}

#[test]
fn rejects_cyclic_trait_prerequisites() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "trait First T where Second T { first: T -> T }\n",
                "trait Second T where First T { second: T -> T }\n",
            ),
            root,
        )
        .expect("cyclic prerequisite source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("prerequisite cycles must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("cyclic trait prerequisite"))
    );
}

#[test]
fn rejects_invalid_multi_parameter_trait_uses_and_members() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Add Left Right Output { add: (Left, Right) -> Output }\n",
            "impl Add I32 I32 { add = pair => 0 }\n",
        )))
        .expect_err_diagnostics("trait implementation arity must match the declaration");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expects 3 compile-time arguments, found 2")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "trait Invalid Left Right { keep_left: Left -> Left }\n",
        ))
        .expect_err_diagnostics("every member must mention every trait parameter");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("must mention trait parameter `Right`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert (From, To) { convert: From -> To }\n",
            "impl Convert I32 { convert = value => value }\n",
        )))
        .expect_err_diagnostics("product binders require product arguments");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("compile-time product parameter requires a product type argument")
    }));
}

#[test]
fn rejects_invalid_traits_implementations_and_unpropagated_bounds() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment I32 { }\n",
        )))
        .expect_err_diagnostics("implementations must be complete");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("missing member `increment`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "type Number = I32\n",
            "impl Increment I32 { increment = value => value }\n",
            "impl Increment Number { increment = value => value }\n",
        )))
        .expect_err_diagnostics("aliases may not create overlapping implementations");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment I32 { increment = value => \"wrong\" }\n",
        )))
        .expect_err_diagnostics("implementation bodies must match their trait member types");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected `I32`, found `String`")),
        "{diagnostics:?}"
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment I32 { increment = value => value }\n",
            "def invalid: <T> T -> T = <T> value => increment value\n",
        )))
        .expect_err_diagnostics("generic callers must propagate trait bounds");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("no trait implementation or matching bound")
    }));
}

#[test]
fn rejects_invalid_trait_member_signatures_and_non_concrete_targets() {
    let diagnostics = TypeChecker::new()
        .check(resolve("trait Invalid T { value: I32 }\n"))
        .expect_err_diagnostics("trait members must be functions that mention the parameter");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("function types"))
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("must mention trait parameter"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment _ { increment = value => value }\n",
        )))
        .expect_err_diagnostics("implementation targets must be concrete");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("target must be fully concrete") })
    );
}

#[test]
fn generic_conditional_trait_implementation_dispatches_and_compiles() {
    let module = type_check(concat!(
        "trait Bound T { check: T -> Bool }\n",
        "trait Target T { act: T -> Bool }\n",
        "impl Bound I32 { check = value => True }\n",
        "impl <T where Bound T> Target T { act = value => Bound.check value }\n",
        "let answer: Bool = Target.act 41\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a conditional trait implementation should monomorphize and compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn rejects_generic_trait_implementation_dispatch_when_bound_is_unmet() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl <T where Bound T> Target T { act = value => value }\n",
            "let answer: I32 = Target.act 41\n",
        )))
        .expect_err_diagnostics(
            "I32 does not implement Bound, so the conditional impl must not apply",
        );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("no trait implementation or matching bound")
    }));
}

#[test]
fn rejects_alpha_equivalent_duplicate_generic_trait_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl <T where Bound T> Target T { act = value => value }\n",
            "impl <U where Bound U> Target U { act = value => value }\n",
        )))
        .expect_err_diagnostics(
            "alpha-equivalent generic implementations must be rejected as duplicates",
        );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));
}

#[test]
fn rejects_a_blanket_implementation_that_overlaps_an_earlier_concrete_one_before_any_call_site() {
    // Once `Bound I32` holds, the blanket `impl<T where Bound T> Target T`
    // applies to `I32` exactly as much as the concrete `impl Target I32`
    // does. This coherence violation is now caught eagerly at the second
    // impl's declaration (`implementation_headers_overlap`) rather than
    // waiting for a call site to expose the ambiguity, so this test no
    // longer needs a `Target.act` call to trigger a diagnostic at all.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl Bound I32 { check = value => True }\n",
            "impl Target I32 { act = value => value }\n",
            "impl <T where Bound T> Target T { act = value => value }\n",
        )))
        .expect_err_diagnostics(
            "a concrete impl and an applicable blanket impl must be rejected as overlapping",
        );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));
}

#[test]
fn rejects_sum_implementations_that_overlap_through_a_row_parameter() {
    // `Result<T, E>` is `Ok<T> | E`, and `E` may be `Never`, one alternative,
    // or a sum, in whichever order the alternatives sort.
    for concrete in [
        "Ok<I32>",
        "Ok<I32> | String",
        "Ok<I32> | Bool",
        "Bool | Ok<I32>",
        "Ok<I32> | String | Bool",
        "Ok<I32> | Ok<String>",
    ] {
        let source = format!(
            "trait Try R {{ name: move R -> String }}\n\
             impl<T, E> Try Result<T, E> {{ name = move value => \"generic\" }}\n\
             impl Try ({concrete}) {{ name = move value => \"concrete\" }}\n"
        );
        let diagnostics = TypeChecker::new()
            .check(resolve(&source))
            .expect_err_diagnostics(&format!("`{concrete}` overlaps `Result<T, E>`"));
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("duplicate trait implementation")),
            "`{concrete}` should be rejected as overlapping `Result<T, E>`",
        );
    }
    for (first, second) in [
        ("impl<T> Try (Ok<T> | Bool)", "impl<T, E> Try (Ok<T> | E | Bool)"),
        ("impl<T, E> Try (Ok<T> | E)", "impl<T, E> Try (Ok<T> | E | Bool)"),
        ("impl<T> Try (Ok<T> | String)", "impl<U> Try (Ok<I32> | U)"),
    ] {
        let source = format!(
            "trait Try R {{ name: move R -> String }}\n\
             {first} {{ name = move value => \"first\" }}\n\
             {second} {{ name = move value => \"second\" }}\n"
        );
        let diagnostics = TypeChecker::new()
            .check(resolve(&source))
            .expect_err_diagnostics(&format!("`{first}` overlaps `{second}`"));
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("duplicate trait implementation")),
            "`{first}` should be rejected as overlapping `{second}`",
        );
    }
}

#[test]
fn accepts_disjoint_sum_implementations_and_binds_a_row_parameter_to_a_sum() {
    type_check(concat!(
        "trait Try R { name: move R -> String }\n",
        "impl<T, E> Try Result<T, E> { name = move value => \"generic\" }\n",
        "impl Try (Bool | String) { name = move value => \"concrete\" }\n",
        "impl Try I32 { name = move value => \"single\" }\n",
        "let wide: Ok<I32> | String | Bool = Ok 1\n",
        "let only: Ok<I32> = Ok 1\n",
        "let other: Bool | String = True\n",
        "let names: (String, String, String) = (Try.name wide, Try.name only, Try.name other)\n",
    ));
}

#[test]
fn cyclic_trait_implementation_bound_fails_without_hanging() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Cyclic T { check: T -> Bool }\n",
            "impl <T where Cyclic T> Cyclic T { check = value => True }\n",
            "let answer: Bool = Cyclic.check 41\n",
        )))
        .expect_err_diagnostics(
            "a self-referential bound must fail rather than being accepted coinductively",
        );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("no trait implementation or matching bound")
    }));
}

#[test]
fn requires_qualification_for_ambiguous_trait_methods() {
    let source = concat!(
        "trait Left T { convert: T -> T }\n",
        "trait Right T { convert: T -> T }\n",
        "impl Left I32 { convert = value => value }\n",
        "impl Right I32 { convert = value => value }\n",
        "let left: I32 = Left.convert 1\n",
        "let right: I32 = Right.convert 2\n",
        "let ambiguous: I32 = convert 3\n",
    );
    let diagnostics = TypeChecker::new()
        .check(resolve(source))
        .expect_err_diagnostics("unqualified overlapping method names must be ambiguous");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("ambiguous trait method; qualify the trait name")
    }));
}

#[test]
fn constructs_wrapper_and_generic_wrapper_values() {
    let module = type_check(concat!(
        "type UserId wrap I32\n",
        "type Box<T> wrap (value: T)\n",
        "let user: UserId = UserId 42\n",
        "let boxed: Box<I32> = Box (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("constructors should compile as zero-cost conversions");
}

#[test]
fn constructs_and_propagates_singleton_nominal_values() {
    let module = type_check(concat!(
        "pub type Foo\n",
        "pub type Bar\n",
        "let foo: Foo = Foo\n",
        "let Foo() = foo\n",
        "def identity: Foo -> Foo = value => value\n",
        "let choice: Foo | Bar = Foo\n",
        "def select = (value: Foo | Bar) => { let ?Foo() = value; Foo }\n",
        "identity(foo)\n",
        "select(choice)\n",
    ));
    let select = module
        .functions()
        .iter()
        .find(|function| function.name == "select")
        .expect("select function");
    assert!(matches!(
        module
            .type_of_function(select.id)
            .expect("select type")
            .result
            .as_ref(),
        CheckedType::Sum(sum) if sum.alternatives.len() == 2
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("singleton values should generate valid LLVM");
}

#[test]
fn opaque_types_do_not_introduce_values_and_singletons_are_not_callable() {
    let diagnostics = NameResolver::new()
        .resolve(&parse("pub type Secret opaque\nSecret\n").expect("source should parse"))
        .expect_err_diagnostics("opaque type should not introduce a value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown name `Secret`")
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("pub type Foo;\nFoo()\n"))
        .expect_err_diagnostics("singleton value should not be callable");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot call a value of type `Foo`")
    }));
}

#[test]
fn contextually_specializes_first_class_generic_functions() {
    let module = type_check(concat!(
        "def identity: <T> move T -> T = <T> move x => x\n",
        "def apply: (I32 -> I32) -> I32 = f => f 42\n",
        "let int_identity: I32 -> I32 = identity\n",
        "let answer: I32 = apply identity\n",
        "let other: I32 = int_identity 7\n",
        "def apply_pair: (I32 -> I32, I32) -> I32 = (f, x) => f x\n",
        "let paired: I32 = apply_pair (identity, 9)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("contextually specialized generic functions should compile");
}

#[test]
fn contextually_specializes_first_class_constructors() {
    let module = type_check(concat!(
        "type UserId wrap I32\n",
        "type Box<T> wrap (value: T)\n",
        "let make_user: I32 -> UserId = UserId\n",
        "let make_box: (value: I32) -> Box<I32> = Box\n",
        "let user: UserId = make_user 42\n",
        "let boxed: Box<I32> = make_box (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("first-class constructors should compile");
}

#[test]
fn destructures_nominal_values_in_lets_and_functions() {
    let module = type_check(concat!(
        "type UserId wrap I32\n",
        "type PairIds wrap (UserId, UserId)\n",
        "let user: UserId = UserId 42\n",
        "let UserId raw = user\n",
        "let pair: PairIds = PairIds (UserId 1, UserId 2)\n",
        "let PairIds (UserId first, UserId second) = pair\n",
        "def unwrap: UserId -> I32 = UserId id => id\n",
        "def get_raw: () -> I32 = () => raw\n",
        "let answer: I32 = unwrap user\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nominal destructuring should be zero-cost");
}

#[test]
fn accesses_visible_nominal_representations_explicitly_and_by_shortcut() {
    let module = type_check(concat!(
        "type User wrap (name: String, age: I32)\n",
        "type Inner wrap (name: String, tag: I32)\n",
        "type Outer wrap Inner\n",
        "let mut user = User (name: \"Ada\", age: 42)\n",
        "let inner: (name: String, age: I32) = user.*\n",
        "let name: String = user.*.name\n",
        "let age: I32 = user.age\n",
        "user.*.age = 43\n",
        "let outer = Outer (Inner (name: \"Grace\", tag: 1))\n",
        "let nested_name: String = outer.*.*.name\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("representation projections should be zero-cost and preserve places");
}

#[test]
fn representation_access_requires_a_nominal_value_and_unwraps_one_shortcut_layer() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Inner wrap (name: String, tag: I32)\n",
            "type Outer wrap Inner\n",
            "let outer = Outer (Inner (name: \"Ada\", tag: 1))\n",
            "let invalid: String = outer.name\n",
            "let also_invalid = (42).*\n",
        )))
        .expect_err_diagnostics("invalid representation projections should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot access an element of `Inner`")
    }));
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected a represented nominal type")
    }));
}

#[test]
fn destructures_contextually_typed_generic_nominal_patterns() {
    let module = type_check(concat!(
        "type Box<T> wrap (value: T)\n",
        "def unbox: <T> move Box<T> -> T = <T> move Box (value) => value\n",
        "let answer: I32 = unbox (Box (value: 42))\n",
        "let text: String = unbox (Box (value: \"hello\"))\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generic nominal patterns should monomorphize");
}

#[test]
fn captures_values_bound_by_nominal_patterns() {
    let module = type_check(concat!(
        "type UserId wrap I32\n",
        "def capture: UserId -> (() -> I32) = user => {\n",
        "  let UserId id = user\n",
        "  () => id\n",
        "}\n",
        "let get_id: () -> I32 = capture (UserId 42)\n",
        "let answer: I32 = get_id ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("destructured leaves should be captured normally");
}

#[test]
fn rejects_moving_fields_out_of_a_borrowed_nominal_destructure() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.list.(List, ListIter)\n",
            "def invalid: ListIter<I32> -> List<I32> = ListIter (list, index) => list\n",
        )))
        .expect_err_diagnostics("a borrowed nominal destructure cannot move its fields out");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot move out of a borrowed value")
    }));
}

#[test]
fn rejects_non_nominal_and_mismatched_nominal_patterns() {
    let syntax = parse(concat!(
        "type Number = I32\n",
        "let Number value = 42\n",
    ))
    .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err_diagnostics("aliases cannot be used as nominal patterns");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("not a represented nominal type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Left wrap I32\n",
            "type Right wrap I32\n",
            "let right: Right = Right 42\n",
            "let Left value = right\n",
        )))
        .expect_err_diagnostics("a nominal pattern must match the same nominal type");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("cannot match"))
    );
}

#[test]
fn monomorphizes_nested_and_recursive_generic_calls() {
    let module = type_check(concat!(
        "def identity: <T> move T -> T = <T> move x => x\n",
        "def copy: <U> move U -> U = <U> move x => identity x\n",
        "def recur: <V> move V -> V = <V> move x => recur x\n",
        "let answer: I32 = copy 42\n",
        "let recurse: I32 = recur 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nested generic calls should discover concrete specializations");
}

#[test]
fn monomorphizes_generic_closures_with_captures() {
    let module = type_check(concat!(
        "def outer: I32 -> I32 = y => {\n",
        "  def inner: <T> T -> I32 = <T> x => y\n",
        "  inner \"ignored\"\n",
        "}\n",
        "let answer: I32 = outer 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("generic closures should retain their captured environment");
}

#[test]
fn infers_product_and_result_only_compile_time_parameters() {
    let module = type_check(concat!(
        "def first: <A, B> (move A, B) -> A = <A, B> (move a, b) => a\n",
        "type Phantom<T> wrap I32\n",
        "def make: <T> I32 -> Phantom<T> = <T> x => Phantom x\n",
        "let answer: I32 = first (42, \"ignored\")\n",
        "let contextual: Phantom<String> = make 7\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("multiple compile-time parameters should specialize");
}

#[test]
fn monomorphizes_curried_generic_function_layers() {
    let module = type_check(concat!(
        "def keep_first: <A, B where Copy A> A -> B -> A = <A, B where Copy A> a => b => a\n",
        "let answer: I32 = keep_first 42 \"ignored\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("curried generic layers should specialize together");
}

#[test]
fn a_default_bound_does_not_imply_default_for_a_product() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def make_list: <T where Default T> () -> (T ^ 32) = <T where Default T> () => {\n",
            "  let list: (T ^ 32) = default ()\n",
            "  list\n",
            "}\n",
        )))
        .expect_err_diagnostics("Default T does not imply Default (T ^ 32)");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation"))
    );
}

#[test]
fn allows_generic_locals_inside_a_nested_closure() {
    let module = type_check(concat!(
        "def make_pair: <T where Default T> () -> (T, T) = <T where Default T> () => {\n",
        "  let build: () -> T = () => {\n",
        "    let value: T = default ()\n",
        "    value\n",
        "  }\n",
        "  (build (), build ())\n",
        "}\n",
        "let pair: (I32, I32) = make_pair ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a nested closure should inherit the enclosing generic scope");
}

#[test]
fn rejects_unconstrained_generic_values_and_non_function_schemes() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def identity: <T> T -> T = <T> x => x\n",
            "def copied = identity\n",
            "def invalid: <U> U = 42\n",
        )))
        .expect_err_diagnostics("generic values require a concrete use or function scheme");
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("requires a concrete expected type"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("function-valued `def`"))
    );
}

#[test]
fn rejects_polymorphic_recursion() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def grow: <T> T -> T = <T> x => grow (x, x)\n"))
        .expect_err_diagnostics("recursive calls may not change their specialization");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("polymorphic recursion")),
        "{diagnostics:?}"
    );
}

#[test]
fn infers_explicit_returns_and_keeps_trailing_expression_semicolons_value_preserving() {
    let module = type_check(concat!(
        "def explicit = () => { return 42; \"unreachable\"; };\n",
        "def implicit = () => { 42; };\n",
        "def binding_ended = () => { let value = 42; };\n",
        "def returned_unit = () => { return (); };\n",
        "explicit ();\n",
    ));

    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "explicit" | "implicit"))
    {
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("function should have a checked type")
                .result,
            CheckedType::I32,
        );
    }
    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "binding_ended" | "returned_unit"))
    {
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("function should have a checked type")
                .result,
            CheckedType::empty_product(),
        );
    }
}

#[test]
fn returns_from_nested_expression_blocks() {
    let module = type_check(concat!(
        "def identity = (value: I32) => value;\n",
        "def answer = () => { identity { return 42; }; 0; };\n",
        "def binding = () => { let unreachable: String = { return 7; }; 0; };\n",
        "answer ();\n",
        "binding ();\n",
    ));
    let answer = module
        .functions()
        .iter()
        .find(|function| function.name == "answer")
        .expect("answer should resolve");
    assert_eq!(
        *module
            .type_of_function(answer.id)
            .expect("answer should be checked")
            .result,
        CheckedType::I32,
    );
    let binding = module
        .functions()
        .iter()
        .find(|function| function.name == "binding")
        .expect("binding should resolve");
    assert_eq!(
        *module
            .type_of_function(binding.id)
            .expect("binding should be checked")
            .result,
        CheckedType::I32,
    );

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nested return should generate valid LLVM");
    assert!(llvm.contains("ret i32 42"));
}

#[test]
fn rejects_returns_outside_functions_and_incompatible_return_values() {
    let outside = parse("return 1\n").expect("return item should parse");
    let diagnostics = NameResolver::new()
        .resolve(&outside)
        .expect_err_diagnostics("top-level return should not resolve");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("only allowed inside a function")
    }));

    let module = resolve("def invalid = () => String :: { return 42; }\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err_diagnostics("return value should match the function result");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `String`, found `I32`")
    }));
}

#[test]
fn supports_contextual_literals_and_arithmetic_for_all_integer_types() {
    let source = concat!(
        "def i8_value = () => I8 :: { let a: I8 = 8; let b: I8 = 4; (a + b) * b - a / b; }\n",
        "def i16_value = () => I16 :: { let a: I16 = 8; let b: I16 = 4; (a + b) * b - a / b; }\n",
        "def i32_value = () => I32 :: { let a: I32 = 8; let b: I32 = 4; (a + b) * b - a / b; }\n",
        "def i64_value = () => I64 :: { let a: I64 = 8; let b: I64 = 4; (a + b) * b - a / b; }\n",
        "def u8_value = () => U8 :: { let a: U8 = 8; let b: U8 = 4; (a + b) * b - a / b; }\n",
        "def u16_value = () => U16 :: { let a: U16 = 8; let b: U16 = 4; (a + b) * b - a / b; }\n",
        "def u32_value = () => U32 :: { let a: U32 = 8; let b: U32 = 4; (a + b) * b - a / b; }\n",
        "def u64_value = () => U64 :: { let a: U64 = 8; let b: U64 = 4; (a + b) * b - a / b; }\n",
        "def isize_value = () => ISize :: { let a: ISize = 8; let b: ISize = 4; (a + b) * b - a / b; }\n",
        "def usize_value = () => USize :: { let a: USize = 8; let b: USize = 4; (a + b) * b - a / b; }\n",
        "i8_value ()\n",
    );
    let module = type_check(source);

    let expected = [
        ("i8_value", CheckedType::I8),
        ("i16_value", CheckedType::I16),
        ("i32_value", CheckedType::I32),
        ("i64_value", CheckedType::I64),
        ("u8_value", CheckedType::U8),
        ("u16_value", CheckedType::U16),
        ("u32_value", CheckedType::U32),
        ("u64_value", CheckedType::U64),
        ("isize_value", CheckedType::ISize),
        ("usize_value", CheckedType::USize),
    ];
    for (name, expected_type) in expected {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        assert_eq!(
            *module.type_of_function(function.id).unwrap().result,
            expected_type
        );
    }

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("all integer arithmetic should generate valid LLVM");
}

#[test]
fn rejects_out_of_range_and_mixed_integer_arithmetic() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let too_large: U8 = 256\n"))
        .expect_err_diagnostics("an out-of-range literal should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("integer literal `256` does not fit in `U8`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def mixed = (left: I8, right: I16) => left + right\n",
            "mixed (1, 2)\n",
        )))
        .expect_err_diagnostics("mixed integer arithmetic should require an explicit conversion");
    assert!(!diagnostics.is_empty());
}

#[test]
fn the_coroutines_example_type_checks() {
    // `examples/coroutines.sta` is the worked manual-scheduler walkthrough; keep
    // it compiling as the surface evolves.
    type_check(include_str!("../examples/coroutines.sta"));
}

#[test]
fn the_game_loop_adapter_example_type_checks() {
    // `examples/game_loop/` is the host-adapter reference: `game.sta` (reusable
    // combinators) + `main.sta` (a demo). It must keep compiling as one program.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_path(&root.join("staple-compiler/examples/game_loop/main.sta"))
        .expect("the game-loop example should load");
    let resolved = NameResolver::new()
        .resolve_program(program)
        .expect("the game-loop example should resolve");
    TypeChecker::new()
        .check(resolved)
        .expect("the game-loop example should type-check");
}

#[test]
fn emits_a_native_object_file() {
    let module = type_check(include_str!("../examples/hello_world.sta"));
    let context = Context::create();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow the Unix epoch")
        .as_nanos();
    let output = std::env::temp_dir().join(format!("staple-compiler-test-{nonce}.o"));

    CodeGenerator::new(&context)
        .emit_object(&lower_whole(&module), &output, None)
        .expect("native object emission should succeed");
    let length = std::fs::metadata(&output)
        .expect("object should exist")
        .len();
    std::fs::remove_file(&output).expect("temporary object should be removable");

    assert!(length > 0);
}

#[test]
fn infers_copy_and_enforces_affine_moves() {
    type_check(concat!(
        "type Point wrap (I32, I32)\n",
        "def copied = () => { let point = Point (1, 2); let other = point; point; other }\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "def invalid = (move value: CString) => { let moved = value; value }\n",
        )))
        .expect_err_diagnostics("CString must be move-only");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use of moved value"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "def invalid = (value: CString) => { let moved = value; value }\n",
        )))
        .expect_err_diagnostics(
            "a plain non-Copy parameter is an implicit borrow and cannot be moved out of",
        );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot move out of a borrowed value")
    }));
}

#[test]
fn borrowed_curried_closures_are_lexically_scoped() {
    let module = type_check(concat!(
        "type MyString wrap String\n",
        "impl !Copy MyString {}\n",
        "companion MyString {\n",
        "  pub def concat = a: MyString => b: MyString => MyString (a.* + b.*)\n",
        "}\n",
        "def immediate = (left: MyString, right: MyString) => MyString.concat left right\n",
        "def local = (left: MyString, right: MyString) => {\n",
        "  let append = MyString.concat left\n",
        "  append right\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("borrowed curried closures should lower through pointer captures");
    assert!(!llvm.contains("closure.borrow"));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type MyString wrap String\n",
            "impl !Copy MyString {}\n",
            "companion MyString {\n",
            "  pub def concat = a: MyString => b: MyString => MyString (a.* + b.*)\n",
            "}\n",
            "def escape = (value: MyString) => MyString.concat value\n",
        )))
        .expect_err_diagnostics("a borrowed partial application cannot be returned");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("borrowed closure cannot escape")
    }));
}

#[test]
fn borrowed_curried_closures_enforce_escape_and_borrow_conflicts() {
    let mutable = type_check(concat!(
        "type MyString wrap String\n",
        "impl !Copy MyString {}\n",
        "companion MyString {\n",
        "  pub def concat = mut a: MyString => b: MyString => MyString (a.* + b.*)\n",
        "}\n",
        "def local = (mut left: MyString, right: MyString) => {\n",
        "  let append = MyString.concat left\n",
        "  append right\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&mutable))
        .expect("mutable borrowed captures should lower through caller storage");

    for source in [
        concat!(
            "type MyString wrap String\nimpl !Copy MyString {}\n",
            "companion MyString { pub def concat = a: MyString => b: MyString => MyString (a.* + b.*) }\n",
            "def consume = f: (MyString -> MyString) => ()\n",
            "def invalid = value: MyString => consume (MyString.concat value)\n",
        ),
        concat!(
            "type MyString wrap String\nimpl !Copy MyString {}\n",
            "companion MyString { pub def concat = a: MyString => b: MyString => MyString (a.* + b.*) }\n",
            "def take = move value: MyString => ()\n",
            "def invalid = move value: MyString => { let append = MyString.concat value; take value }\n",
        ),
        concat!(
            "type MyString wrap String\nimpl !Copy MyString {}\n",
            "companion MyString { pub def concat = a: MyString => b: MyString => MyString (a.* + b.*) }\n",
            "def invalid = value: MyString => { let concat = MyString.concat; concat value }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("borrowed closures must not escape or conflict with moves");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("borrowed closure")
                || diagnostic.message.contains("borrowed by a closure")
                || diagnostic.message.contains("borrow-producing function")
        }));
    }

    type_check(concat!(
        "type MyString wrap String\nimpl !Copy MyString {}\n",
        "companion MyString { pub def concat = move a: MyString => b: MyString => MyString (a.* + b.*) }\n",
        "def make = move value: MyString => MyString.concat value\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type MyString wrap String\nimpl !Copy MyString {}\n",
            "def combine = (left: MyString, right: MyString) => tail: MyString => MyString (left.* + right.* + tail.*)\n",
            "def take = move value: MyString => ()\n",
            "def invalid = (move left: MyString, move right: MyString) => {\n",
            "  let append = combine (left, right)\n",
            "  take right\n",
            "}\n",
        )))
        .expect_err_diagnostics("every captured parameter must remain borrowed");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("borrowed by a closure") })
    );
}

#[test]
fn exposes_copy_but_rejects_explicit_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Value wrap I32\n",
            "impl Copy Value {}\n",
        )))
        .expect_err_diagnostics("Copy implementations must be inferred");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`Copy` is implemented structurally")
    }));
}

#[test]
fn c_string_to_string_consumes_its_argument() {
    type_check(concat!(
        "use std.cinterop.*\n",
        "def convert = () => { let text = c_string \"x\"; CString.to_string_unchecked text }\n",
        "def temporary = () => CString.to_string_unchecked (c_string \"y\")\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "def twice = () => {\n",
            "  let text = c_string \"x\"\n",
            "  let first = CString.to_string_unchecked text\n",
            "  CString.to_string_unchecked text\n",
            "}\n",
        )))
        .expect_err_diagnostics("a converted CString is moved");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use of moved value")),
        "{diagnostics:?}"
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "def borrowed: CString -> String = text => CString.to_string_unchecked text\n",
        )))
        .expect_err_diagnostics("a borrowed CString cannot be converted");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("cannot move out of a borrowed value")),
        "{diagnostics:?}"
    );
}

#[test]
fn negative_copy_impl_opts_a_nominal_type_out_of_copy_without_drop() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Handle wrap I32\n",
            "impl !Copy Handle {}\n",
            "def invalid = (move value: Handle) => { let moved = value; value }\n",
        )))
        .expect_err_diagnostics("a `!Copy` type must be move-only");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use of moved value"))
    );

    // Declaring `!Copy` alone does not require a `drop` method or add
    // destructor glue: the type is move-only but not finalized.
    type_check(concat!(
        "type Handle wrap I32\n",
        "impl !Copy Handle {}\n",
        "def make = () => Handle 1\n",
    ));
}

#[test]
fn rejects_negative_impl_for_traits_other_than_copy() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Handle wrap I32\n",
            "impl !Drop Handle {}\n",
        )))
        .expect_err_diagnostics("only `Copy` can be negated");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("only `Copy` can be negated"))
    );
}

#[test]
fn rejects_negative_copy_impl_for_non_nominal_types() {
    let diagnostics = TypeChecker::new()
        .check(resolve("impl !Copy I32 {}\n"))
        .expect_err_diagnostics("`!Copy` requires a represented nominal type");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`!Copy` may only be implemented for a represented nominal type")
    }));
}

#[test]
fn generic_drop_selection_rules_are_enforced_at_declaration() {
    // A generic `Drop` implementation beside an exact one is an overlap, so
    // coherence rejects it and selection needs no precedence rule.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Box<T> wrap (T)\n",
            "impl<T> Drop Box<T> { cleanup = Box value => () }\n",
            "impl Drop Box<I32> { cleanup = Box value => () }\n",
        )))
        .expect_err_diagnostics("overlapping Drop implementations are rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));

    // A `Drop` bound may constrain only the implementation's own parameters.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Box<T> wrap (T)\n",
            "impl<T where Copy Box<T>> Drop Box<T> { cleanup = Box value => () }\n",
        )))
        .expect_err_diagnostics("a bound on the header type is rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("bounds may constrain only its own type parameters")
    }));

    // A template with a possibly-applicable `Drop` implementation is not
    // `Copy`, so copying it is a move error.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Box<T> wrap (T)\n",
            "impl<T> Drop Box<T> { cleanup = Box value => () }\n",
            "def dup: <T> Box<T> -> (Box<T>, Box<T>) = <T> value => (value, value)\n",
        )))
        .expect_err_diagnostics("a generically droppable type is move-only");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot move out of a borrowed value")
    }));
}

#[test]
fn lowers_custom_drop_and_gc_finalizer_glue() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Resource wrap I32\n",
        "impl Drop Resource { cleanup = Resource value => () }\n",
        "def release = () => { let handle = Resource 7; drop handle }\n",
        "def managed = () => Shared (Resource 9)\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("Drop glue should compile");

    assert!(llvm.contains("drop.call"));
    assert!(llvm.contains("__staple_gc_set_finalizer"));
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::GcFinalizer);
}

#[test]
fn rejects_partial_moves_and_cstring_returns_from_c() {
    let ffi_diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { invalid_return: () -> CString }\n",
        )))
        .expect_err_diagnostics("C must not manufacture owned CStrings");
    assert!(
        ffi_diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("cannot return owned `CString`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "def partial = (pair: (CString, I32)) => pair.0\n",
        )))
        .expect_err_diagnostics("unsupported ownership operations should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("cannot move a field"))
    );
}

#[test]
fn allows_move_only_globals_borrowed_from_top_level_statements_and_functions() {
    let module = type_check(concat!(
        "use std.io.println\n",
        "let mut numbers: List<I32> = List.new ()\n",
        "List.push numbers 1\n",
        "List.push numbers 2\n",
        "def push_three = () => List.push numbers 3\n",
        "push_three ()\n",
        "println (ToString.to_string (List.length numbers))\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("borrowing a move-only global should compile");
}

#[test]
fn rejects_moving_a_mut_parameter_out_without_reassigning_it() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.buffer.Buffer\n",
            "def leak = (mut buffer: Buffer<I32>; move extra: I32) => Buffer<I32> :: buffer\n",
        )))
        .expect_err_diagnostics("a moved-out mut parameter must be reassigned");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("must be reassigned before the function returns")
    }));
}

#[test]
fn accepts_moving_a_mut_parameter_out_when_it_is_reassigned() {
    type_check(concat!(
        "use std.buffer.Buffer\n",
        "def regrow = (mut buffer: Buffer<I32>) => () :: {\n",
        "let mut bigger: Buffer<I32> = Buffer.with_capacity (USize :: 8)\n",
        "Buffer.transfer buffer bigger\n",
        "buffer = bigger\n",
        "}\n",
    ));
}

#[test]
fn rejects_moving_a_move_only_global_out_of_top_level_statements() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.buffer.Buffer\nuse std.shared_buffer.SharedBuffer\n",
            "let mut data: Buffer<I32> = Buffer.with_capacity (USize :: 4)\n",
            "let frozen: SharedBuffer<I32> = Buffer.into_shared_buffer data\n",
        )))
        .expect_err_diagnostics("moving a global out at top level should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot move a value out of a global binding")
    }));
}

#[test]
fn rejects_moving_a_move_only_global_out_of_a_function() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.buffer.Buffer\n",
            "let mut data: Buffer<I32> = Buffer.with_capacity (USize :: 4)\n",
            "def take = () => Buffer.into_shared_buffer data\n",
        )))
        .expect_err_diagnostics("moving a global out of a function should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot move a value out of a global binding")
    }));
}

#[test]
fn moves_resources_into_managed_closures_and_borrows_shared_payloads() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { inspect: CString -> I32 }\n",
        "def make = (move value: CString) => { let callback = () => inspect value; callback }\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("a move-only capture should get managed finalizer glue");
    assert_artifact_definition(&llvm, &lowered, PlannedArtifactFamily::GcClosureFinalizer);

    for source in [
        concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "def invalid = (move value: CString) => { let callback = () => inspect value; value }\n",
        ),
        concat!(
            "use std.shared.(Shared)\n",
            "use std.cinterop.*\n",
            "def invalid = (value: Shared<CString>) => { let Shared inner = value; drop inner }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("captured and Shared-owned resources cannot be moved again");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("moved value")
                || diagnostic
                    .message
                    .contains("cannot move out of a borrowed value")
        }));
    }
}

#[test]
fn lowers_path_sensitive_drop_flags() {
    let module = type_check(concat!(
        "type Resource wrap I32\n",
        "impl Drop Resource { cleanup = Resource value => () }\n",
        "def conditional = (flag: Bool, move handle: Resource) => match flag {\n",
        "  True() => { drop handle; () },\n",
        "  False() => (),\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("conditional ownership should lower through a runtime drop flag");
    assert!(llvm.contains("drop.is_live"));
    assert!(llvm.contains("drop.call"));
}

#[test]
fn supports_contextual_string_literal_types_and_widening() {
    let module = type_check(concat!(
        "type Answer = \"yes\" | \"no\"\n",
        "def inferred = () => \"yes\"\n",
        "def narrow: () -> Answer = () => \"yes\"\n",
        "let answer: Answer = narrow()\n",
        "let widened: String = answer\n",
        "let absorbed: \"yes\" | String = \"anything\"\n",
    ));
    let inferred = module
        .functions()
        .iter()
        .find(|function| function.name == "inferred")
        .expect("inferred function");
    assert_eq!(
        *module.type_of_function(inferred.id).unwrap().result,
        CheckedType::String
    );
    let narrow = module
        .functions()
        .iter()
        .find(|function| function.name == "narrow")
        .expect("narrow function");
    assert_eq!(
        *module.type_of_function(narrow.id).unwrap().result,
        CheckedType::StringLiteralSet(vec!["no".to_owned(), "yes".to_owned()])
    );
}

#[test]
fn rejects_invalid_string_literal_narrowing() {
    for (source, expected) in [
        (
            "def invalid: () -> \"yes\" | \"no\" = () => \"maybe\"\n",
            "expected `\"no\" | \"yes\"`, found `String`",
        ),
        (
            "let broad: String = \"yes\"\nlet narrow: \"yes\" | \"no\" = broad\n",
            "expected `\"no\" | \"yes\"`, found `String`",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("invalid literal refinement should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing `{expected}` in {diagnostics:?}",
        );
    }
}

#[test]
fn matches_literal_sets_strings_and_mixed_nominal_unions() {
    let module = type_check(concat!(
        "type Some wrap String\n",
        "def pure: (\"yes\" | \"no\") -> String = value => match value {\n",
        "  \"yes\" => \"affirmative\",\n",
        "  \"no\" => \"negative\",\n",
        "}\n",
        "def broad: String -> String = value => match value {\n",
        "  \"yes\" => \"affirmative\",\n",
        "  _ => \"other\",\n",
        "}\n",
        "def mixed: Some | \"yes\" | \"no\" -> String = value => match value {\n",
        "  Some text => text,\n",
        "  \"yes\" => \"affirmative\",\n",
        "  \"no\" => \"negative\",\n",
        "}\n",
        "let small: \"yes\" = \"yes\"\n",
        "let larger: \"yes\" | \"no\" = small\n",
        "let injected: Some | \"yes\" | \"no\" = larger\n",
        "pure small\n",
        "broad \"other\"\n",
        "mixed injected\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("literal and mixed matches should generate LLVM");
    assert!(llvm.contains("match.string.length_matches"));
    assert!(llvm.contains("@memcmp"));
    assert!(llvm.contains("sum.target.tag"));
}

#[test]
fn checks_literal_match_exhaustiveness_and_reachability() {
    let missing = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid: (\"yes\" | \"no\") -> String = value => match value {\n",
            "  \"yes\" => \"only\",\n",
            "}\n",
        )))
        .expect_err_diagnostics("a literal alternative is missing");
    assert!(
        missing
            .iter()
            .any(|diagnostic| diagnostic.message.contains("non-exhaustive match"))
    );

    let duplicate = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid: String -> String = value => match value {\n",
            "  \"yes\" => \"first\",\n",
            "  \"yes\" => \"second\",\n",
            "  _ => \"other\",\n",
            "}\n",
        )))
        .expect_err_diagnostics("a duplicate literal arm is unreachable");
    assert!(
        duplicate
            .iter()
            .any(|diagnostic| diagnostic.message == "unreachable match arm")
    );
}

#[test]
fn rejects_refutable_string_literal_binding_patterns() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid: String -> String = \"yes\" => \"matched\"\n",
        )))
        .expect_err_diagnostics("literal function patterns must be irrefutable");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("string literal binding pattern must have the same singleton literal type")
    }));

    type_check("def valid: \"yes\" -> String = \"yes\" => \"matched\"\n");
}

#[test]
fn infers_and_generates_loop_result_values() {
    let module = type_check(concat!(
        "def answer = () => loop { break 42 }\n",
        "def nested = () => loop { loop { break () }; break 7 }\n",
        "def unit = () => loop { break }\n",
        "answer ()\n",
        "nested ()\n",
        "unit ()\n",
    ));
    for (name, expected) in [
        ("answer", CheckedType::I32),
        ("nested", CheckedType::I32),
        ("unit", CheckedType::empty_product()),
    ] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .expect("loop function should resolve");
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("loop function should be typed")
                .result,
            expected,
        );
    }
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("loops should generate valid LLVM");
    assert!(llvm.contains("loop.body"));
    assert!(llvm.contains("loop.exit"));
    assert!(llvm.contains("loop.value"));
}

#[test]
fn supports_loop_control_through_matches_and_function_returns() {
    let module = type_check(concat!(
        "def select: Bool -> I32 = condition => loop {\n",
        "  match condition { True() => { break 9 }, False() => { continue } }\n",
        "}\n",
        "def early: Bool -> I32 = condition => loop {\n",
        "  match condition { True() => { return 5 }, False() => { break 6 } }\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nested loop control should generate valid LLVM");
}

#[test]
fn rejects_invalid_loop_control_and_break_types() {
    for (source, expected) in [
        ("break 1\n", "`break` is only allowed inside a loop"),
        ("continue\n", "`continue` is only allowed inside a loop"),
        (
            "def invalid = () => loop { def nested = () => { break }; break () }\n",
            "`break` is only allowed inside a loop",
        ),
    ] {
        let diagnostics = NameResolver::new()
            .resolve(&parse(source).expect("invalid loop control should still parse"))
            .expect_err_diagnostics("loop control should fail resolution");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing `{expected}` in {diagnostics:?}",
        );
    }

    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid: () -> I32 = () => loop { break }\n"))
        .expect_err_diagnostics("unit break should not satisfy I32");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("expected `I32`") && diagnostic.message.contains("found `()`")
    }));
}

#[test]
fn accepts_divergent_loops_in_typed_contexts() {
    let module = type_check("def forever: () -> I32 = () => loop { continue }\n");
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a divergent loop should satisfy its expected type");
}

#[test]
fn supports_never_as_the_bottom_type_and_tracks_divergence() {
    let module = type_check(concat!(
        "def forever: () -> Never = () => loop { continue }\n",
        "def inferred = () => loop { continue }\n",
        "def coerced: () -> I32 = () => forever ()\n",
        "def caller = () => { forever (); 42 }\n",
        "def normalized: () -> (Never | I32) = () => 8\n",
        "def joined: Bool -> I32 = condition => loop {\n",
        "  match condition {\n",
        "    True() => { break forever () },\n",
        "    False() => { break 7 },\n",
        "  }\n",
        "}\n",
    ));

    for (name, expected) in [
        ("forever", CheckedType::Never),
        ("inferred", CheckedType::Never),
        ("coerced", CheckedType::I32),
        ("caller", CheckedType::Never),
        ("normalized", CheckedType::I32),
        ("joined", CheckedType::I32),
    ] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .expect("Never test function should resolve");
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("Never test function should be typed")
                .result,
            expected,
        );
    }

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("Never functions and calls should generate valid LLVM");
    assert!(llvm.contains("unreachable"));
}

#[test]
fn panic_diverges_and_unifies_with_any_expected_type() {
    let module = type_check(concat!(
        "use std.process.panic\n",
        "def choose: Bool -> I32 = cond => match cond {\n",
        "  True() => 1,\n",
        "  False() => panic \"unreachable\",\n",
        "}\n",
    ));
    let function = module
        .functions()
        .iter()
        .find(|function| function.name == "choose")
        .expect("choose should resolve");
    assert_eq!(
        *module
            .type_of_function(function.id)
            .expect("choose should be typed")
            .result,
        CheckedType::I32,
    );

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("panic call should generate valid LLVM");
    assert!(llvm.contains("unreachable"));
}

#[test]
fn a_diverging_top_level_statement_does_not_double_terminate_its_module_initializer() {
    // A `Never`-typed expression already closes its basic block with
    // `unreachable`; `compile_module_initializers` used to then
    // unconditionally emit a trailing `ret void` in the same block
    // regardless, producing an invalid module ("Terminator found in the
    // middle of a basic block") whenever the *last* top-level statement of
    // a module diverged.
    let module = type_check(concat!(
        "def forever: () -> Never = () => loop { continue }\n",
        "forever ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a diverging top-level statement should compile to a valid module");
    assert!(llvm.contains("unreachable"));
}

#[test]
fn deduplicates_matching_extern_declarations_across_modules() {
    // `std.process`'s `panic` is prelude-ambient and transitively declares
    // the raw C `exit` symbol internally; a user program that also
    // declares a matching `extern "c" { exit: ... }` must reuse that one
    // declaration rather than have LLVM silently rename the second one
    // (`@exit.1`), detaching it from the real libc symbol at link time.
    let module = type_check(concat!("extern \"c\" { exit: I32 -> () }\n", "exit 5\n",));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a matching duplicate extern declaration should be deduplicated");
    assert!(
        !llvm.contains("@exit.1"),
        "the second `exit` declaration should reuse the first, not rename itself:\n{llvm}",
    );
}

#[test]
fn rejects_external_function_overloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "extern \"c\" { foreign: I32 -> I32\n foreign: (I32; I32) -> I32\n}\n",
            root,
        )
        .expect("source loads");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("duplicate names must be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("duplicate definition of `foreign`")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_conflicting_extern_declarations_of_the_same_symbol() {
    let module = type_check(concat!(
        "extern \"c\" { exit: I32 -> I32 }\n",
        "let _ = exit 5\n",
    ));
    let context = Context::create();
    let diagnostics = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect_err("a conflicting extern redeclaration of `exit` should be rejected");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("already declared with a different type")),
        "{diagnostics:?}",
    );
}

#[test]
fn checks_reachability_correctly_after_a_top_level_loop_with_break() {
    // A top-level `loop { ...; break }` (here via the `for` macro's
    // expansion) must not poison reachability for the rest of the module:
    // `total` is genuinely used afterward, and `to_string` is a
    // trait-method-shorthand whose overload resolution is skipped for
    // code the checker (wrongly) considers unreachable, so this exercises
    // both symptoms at once.
    let source = concat!(
        "let mut total = 0\n",
        "for value in (0 ..= 2) {\n",
        "  total = total + value\n",
        "}\n",
        "let text: String = to_string total\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("code after a top-level loop with a break should still be checked and generated");
    assert!(llvm.contains("loop.body"));
}

#[test]
fn checks_ownership_across_loop_exits_and_back_edges() {
    let module = type_check(concat!(
        "type Resource wrap I32\n",
        "impl Drop Resource { cleanup = Resource _ => () }\n",
        "def choose: Bool -> Resource = condition => loop {\n",
        "  let value = Resource 1\n",
        "  match condition {\n",
        "    True() => { break value },\n",
        "    False() => { continue },\n",
        "  }\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("loop exits should preserve moved results and drop iteration locals");
    assert!(llvm.contains("loop.value"));
    assert!(llvm.contains("drop.call"));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Resource wrap I32\n",
            "impl Drop Resource { cleanup = Resource _ => () }\n",
            "def invalid = (move value: Resource) => loop {\n",
            "  let consumed = value\n",
            "  continue\n",
            "}\n",
        )))
        .expect_err_diagnostics("an outer move-only value cannot be consumed before a back-edge");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("may be moved before the next loop iteration")
    }));
}

#[test]
fn pub_repr_type_constructor_satisfies_its_own_subtype_bound() {
    let module = type_check(concat!(
        "pub type Ident2<Spelling where Spelling <: String> wrap pub Spelling\n",
        "let literal: Ident2<String> = Ident2 \"answer\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("wrapper type whose representation is a bare bounded parameter should compile");
}

#[test]
fn checks_and_generates_short_circuiting_logical_operators() {
    let module = type_check(concat!(
        "def positive_and_small: I32 -> Bool = n => n > 0 && n < 10\n",
        "def zero_or_one: I32 -> Bool = n => n == 0 || n == 1\n",
        // `&&` binds tighter than `||`, and both are left-associative chains
        // rather than the non-associative comparison/range operators.
        "def chained: I32 -> Bool = n => n > 0 && n < 10 || n == 20 || n == 21\n",
        // Bare `True`/`False` literals must not hit the singleton-narrowing
        // that a hand-written `match True { ... }` would.
        "def literals: () -> Bool = () => True && False || True\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("`&&`/`||` should compile to short-circuiting branches");
    assert!(llvm.contains("logical.short_circuit"));
    assert!(llvm.contains("logical.right"));
    assert!(llvm.contains("logical.merge"));
}

#[test]
fn rejects_non_bool_logical_operands() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def bad: (I32, I32) -> I32 = (a, b) => a && b\n"))
        .expect_err_diagnostics("`&&` requires `Bool` operands, and is not overloadable for `I32`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `Bool`, found `I32`"))
    );
}

#[test]
fn folds_const_arithmetic_and_recursive_calls_at_compile_time() {
    let module = type_check(concat!(
        "const x: I32 = 1 + 3\n",
        "def fibonacci: I32 -> I32 = n =>\n",
        "    match n < 2 {\n",
        "        True() => n,\n",
        "        False() => fibonacci (n - 1) + fibonacci (n - 2),\n",
        "    }\n",
        "const y = fibonacci 10\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("const bindings should fold and compile");
    // `x` and `y` are folded to plain literals by the compiler, not
    // computed by generated code at program startup: the initializer
    // functions store the already-computed constants directly, with no
    // trace of `fibonacci`'s recursive calls in `y`'s initialization.
    assert!(llvm.contains("i32 4"));
    assert!(llvm.contains("i32 55"));
}

#[test]
fn folds_const_strings_and_products() {
    let module = type_check(concat!(
        "const greeting: String = \"hello, const!\"\n",
        "const point: (x: I32, y: I32) = (x: 1 + 1, y: 2 + 2)\n",
        "def read_point: () -> I32 = () => point.x + point.y\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("string and product consts should fold and compile");
    assert!(llvm.contains("hello, const!"));
}

#[test]
fn folds_const_float_arithmetic_at_compile_time() {
    let module = type_check(concat!(
        "const x: F64 = 1.5 + 2.5\n",
        "const y: F64 = 10.0 / 4.0\n",
        "const z: F64 = 1.0 - 3.0\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("float const bindings should fold and compile");
    // All three fold directly to literal constants. A negative result is a
    // compiler-built literal even though source literals cannot carry a
    // leading `-`.
    assert!(llvm.contains("double 4.000000e+00"));
    assert!(llvm.contains("double 2.500000e+00"));
    assert!(llvm.contains("double -2.000000e+00"));
}

#[test]
fn folds_const_float_operations_at_compile_time() {
    let module = type_check(concat!(
        "const floored: F64 = F64.floor 1.9\n",
        "const ceiled: F64 = F64.ceil 1.1\n",
        "const truncated: F64 = F64.trunc 1.9\n",
        "const rounded: F64 = F64.round 1.5\n",
        "const even: F64 = F64.round_even 2.5\n",
        "const rooted: F64 = F64.sqrt 9.0\n",
        "const smallest: F64 = F64.min 2.0 3.0\n",
        "const bits: U64 = F64.to_bits 1.0\n",
        "const restored: F64 = F64.from_bits bits\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("tier-1 float operations should fold and compile");
    let value = |name: &str| {
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("binding `{name}` should exist"))
    };
    let literal = |expression: &staple_syntax::Expression| match expression {
        staple_syntax::Expression::Float(float) => float.literal.clone(),
        other => panic!("expected a folded float literal, found {other:?}"),
    };
    assert_eq!(literal(&value("floored")), "1.0");
    assert_eq!(literal(&value("rounded")), "2.0");
    assert_eq!(literal(&value("rooted")), "3.0");
}

#[test]
fn remainders_compile_with_integer_and_float_lowering() {
    let module = type_check(concat!(
        "let signed: I32 = ((I32 :: 0) - (I32 :: 7)) % (I32 :: 3)\n",
        "let unsigned: U32 = (U32 :: 7) % (U32 :: 3)\n",
        "let float: F64 = (F64 :: 7.5) % (F64 :: 2.0)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .expect("remainders should compile");
    assert!(llvm.contains("srem"), "signed remainder uses srem:\n{llvm}");
    assert!(
        llvm.contains("urem"),
        "unsigned remainder uses urem:\n{llvm}"
    );
    assert!(
        llvm.contains("frem"),
        "float remainder uses frem:\n{llvm}"
    );
    // The zero-divisor guard lives in the `Remainder` implementations, which
    // panic at the caller rather than reaching the raw intrinsic.
    assert!(
        llvm.contains("remainder by zero"),
        "a zero divisor panics instead of being undefined behavior:\n{llvm}"
    );
}

#[test]
fn folded_float_constants_keep_their_float_type() {
    let module = type_check(concat!(
        "const infinity = F32.Infinity\n",
        "const pi = F32.Pi\n",
        "const ascribed = F32 :: 1.5\n",
        "const pair: (F32, F32) = (0.0 / 0.0, 1.0)\n",
        "let a: F32 = infinity\n",
        "let b: F32 = pi\n",
        "let c: F32 = ascribed\n",
        "let d: (F32, F32) = pair\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("F32 constants should fold as F32 and compile");
}

#[test]
fn folded_float_constants_read_companions_through_renamed_imports() {
    let module = type_check(concat!(
        "use std.number.(F64 as Real)\n",
        "const pi: Real = Real.Pi\n",
        "const floored: Real = Real.floor 2.5\n",
    ));
    let value = |name: &str| {
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("binding `{name}` should exist"))
    };
    for (name, expected) in [("pi", "3.141592653589793"), ("floored", "2.0")] {
        match value(name) {
            staple_syntax::Expression::Float(float) => assert_eq!(float.literal, expected),
            other => panic!("expected `{name}` to fold to a literal, found {other:?}"),
        }
    }
}

#[test]
fn folded_float_constants_reject_mixed_float_types() {
    for (source, message) in [
        ("const narrowed: F32 = F64.Pi\n", "expected `F32`, found `F64`"),
        (
            "const mixed = F32.Pi + F64.Pi\n",
            "operator `+` cannot combine `F32` and `F64`",
        ),
    ] {
        let diagnostics = resolve_result(source)
            .expect_err("a mixed float constant should be rejected while folding");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(message)),
            "expected {message:?} for {source:?}, found {diagnostics:?}"
        );
    }
}

#[test]
fn folds_euclidean_float_remainders_inside_their_range() {
    let module = type_check(concat!(
        "const tiny: F64 = F64.remainder_euclid (0.0 - 1.0e-20) 1.0\n",
        "const quotient: F64 = F64.divide_euclid (0.0 - 7.0) 2.0\n",
        "const remainder: F64 = F64.remainder_euclid (0.0 - 7.0) 2.0\n",
    ));
    let value = |name: &str| {
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("binding `{name}` should exist"))
    };
    for (name, expected) in [("tiny", "0.0"), ("quotient", "-4.0"), ("remainder", "1.0")] {
        match value(name) {
            staple_syntax::Expression::Float(float) => assert_eq!(float.literal, expected),
            other => panic!("expected `{name}` to fold to a literal, found {other:?}"),
        }
    }
}

#[test]
fn float_companions_are_reachable_from_pipe_stages_without_an_import() {
    let module = type_check(concat!(
        "let x = 7.5 / 2.0\n",
        "let cell = x |> floor\n",
        "let n = x |> round |> round_even\n",
        "let hypotenuse = (3.0 * 3.0 + 4.0 * 4.0) |> sqrt\n",
        "let lower = x |> min 10.0\n",
        "let clamped = x |> clamp 0.0 10.0\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .expect("float companion pipe stages should compile");
    for intrinsic in [
        "llvm.floor",
        "llvm.round",
        "llvm.roundeven",
        "llvm.sqrt",
        "llvm.minnum",
    ] {
        assert!(
            llvm.contains(intrinsic),
            "expected `{intrinsic}` in emitted IR:\n{llvm}"
        );
    }
}

#[test]
fn float_companions_cover_the_tier_one_toolbox() {
    let module = type_check(concat!(
        "let a = 1.9 |> ceil\n",
        "let b = 1.9 |> trunc\n",
        "let c = -1.5 |> abs\n",
        "let d = 2.5 |> fract\n",
        "let e = 2.0 |> copysign (-1.0)\n",
        "let f = 2.0 |> max 3.0\n",
        "let g = F64.mul_add 2.0 3.0 4.0\n",
        "let h = F64.remainder_euclid (-7.0) 3.0\n",
        "let i = F64.divide_euclid (-7.0) 3.0\n",
        "let j = F64.is_nan F64.NaN\n",
        "let k = F64.is_infinite F64.Infinity\n",
        "let l = F64.is_finite 1.0\n",
        "let m = F64.is_sign_negative F64.NegInfinity\n",
        "let n: U64 = F64.to_bits 1.0\n",
        "let o: F64 = F64.from_bits n\n",
        "let p = F64.Epsilon + F64.MinPositive + F64.Pi + F64.E\n",
        "let q = F64.Min + F64.Max\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .expect("the tier-one float toolbox should compile");
    for intrinsic in [
        "llvm.ceil",
        "llvm.trunc",
        "llvm.fabs",
        "llvm.copysign",
        "llvm.maxnum",
        "llvm.fma",
        "bitcast",
    ] {
        assert!(
            llvm.contains(intrinsic),
            "expected `{intrinsic}` in emitted IR:\n{llvm}"
        );
    }
}

#[test]
fn folds_const_float_initializers_with_ieee_non_finite_results() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "const infinity: F64 = 1.0 / 0.0\n",
                "const nan: F64 = 0.0 / 0.0\n",
                "const negative_infinity: F64 = -1.0 / 0.0\n",
            ),
            root,
        )
        .expect("source should parse");
    let module = NameResolver::new()
        .resolve_program(program)
        .expect("non-finite computed constants should fold under IEEE rules");
    let value = |name: &str| {
        module
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("binding `{name}` should exist"))
    };
    let bits = |expression: &staple_syntax::Expression| match expression {
        staple_syntax::Expression::Call(call) => {
            let staple_syntax::Expression::Name(callee) = call.callee.as_ref() else {
                panic!("expected a bit-cast call, found {call:?}");
            };
            assert_eq!(callee.name, "__f64_from_bits");
            let staple_syntax::Expression::Integer(integer) = call.argument.as_ref() else {
                panic!("expected an integer bit pattern, found {call:?}");
            };
            integer.literal.clone()
        }
        other => panic!("expected a reified non-finite float, found {other:?}"),
    };
    // Positive infinity, quiet NaN, and negative infinity, exactly.
    assert_eq!(bits(&value("infinity")), "9218868437227405312");
    assert_eq!(bits(&value("nan")), "9221120237041090560");
    assert_eq!(bits(&value("negative_infinity")), "18442240474082181120");
}

#[test]
fn folded_nans_have_the_same_bits_on_every_host() {
    // A NaN an operation produces is the canonical quiet NaN, a NaN operand
    // propagates quieted, and `F32` NaNs keep their exact bits, whatever
    // the compiling machine's hardware would produce.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "const signaling: F64 = F64.from_bits 9218868437227405313\n",
                "const propagated: F64 = signaling + 1.0\n",
                "const negative: F64 = F64.from_bits 18444492273895866368 * 2.0\n",
                "const root: F64 = F64.sqrt (-1.0)\n",
                "const magnitude: F64 = F64.abs (F64.from_bits 18444492273895866368)\n",
                "const narrow: F32 = 0.0 / 0.0\n",
                "const narrow_signaling: F32 = F32.from_bits 2139095041\n",
                "const narrow_propagated: F32 = narrow_signaling * 2.0\n",
            ),
            root,
        )
        .expect("source should parse");
    let module = NameResolver::new()
        .resolve_program(program)
        .expect("NaN constants should fold");
    let bits = |name: &str| {
        let value = module
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("binding `{name}` should exist"));
        let staple_syntax::Expression::Call(call) = value else {
            panic!("expected `{name}` to reify as a bit cast, found {value:?}");
        };
        let staple_syntax::Expression::Integer(integer) = call.argument.as_ref() else {
            panic!("expected an integer bit pattern, found {call:?}");
        };
        integer.literal.clone()
    };
    // 0x7FF0000000000001 stays signaling until arithmetic quiets it.
    assert_eq!(bits("signaling"), "9218868437227405313");
    assert_eq!(bits("propagated"), "9221120237041090561");
    // The operand's sign survives, rather than the host's choice.
    assert_eq!(bits("negative"), "18444492273895866368");
    assert_eq!(bits("root"), "9221120237041090560");
    assert_eq!(bits("magnitude"), "9221120237041090560");
    // 0x7FC00000, 0x7F800001 and its quieted 0x7FC00001.
    assert_eq!(bits("narrow"), "2143289344");
    assert_eq!(bits("narrow_signaling"), "2139095041");
    assert_eq!(bits("narrow_propagated"), "2143289345");
}

#[test]
fn still_rejects_float_literals_that_overflow() {
    let resolved =
        resolve_result("const x: F64 = 1e999\n").expect("the literal should reach the checker");
    let diagnostics = TypeChecker::new()
        .check(resolved)
        .expect_err_diagnostics("an overflowing float literal is still rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not fit in `F64`"))
    );
}

#[test]
fn rejects_const_initializers_that_cannot_be_folded_to_a_compile_time_constant() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("const f = () => 1\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("a function value cannot be a compile-time constant");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot be represented as a compile-time constant")
    }));
}

#[test]
fn rejects_self_referential_const_bindings() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("const x = x + 1\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("a self-referential const should be rejected, not hang or crash");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("recursed too deeply"))
    );
}

#[test]
fn resolves_local_const_names_before_their_textual_position_like_def() {
    // Local bindings' types aren't seeded ahead of time for forward
    // reference — a pre-existing limitation shared by local `def` — so
    // this still fails to *type-check*. What this asserts is that name
    // *resolution* succeeds (the `resolve_block` hoisting change in
    // `resolve.rs` finds `later` at all, the same way it already does for
    // `def`), rather than failing earlier with "unknown name `later`".
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "def wrapper: () -> I32 = () => {\n",
                "    let result = later\n",
                "    const later: I32 = 1 + 2\n",
                "    result\n",
                "}\n",
            ),
            root,
        )
        .expect("source should parse");
    let resolved = NameResolver::new()
        .resolve_program(program)
        .expect("a hoisted local const's name should resolve, like a local def's");
    let diagnostics = TypeChecker::new().check(resolved).expect_err_diagnostics(
        "reading a local binding before its initializer still fails to type-check",
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("later"))
    );
}

#[test]
fn as_syntax_requires_an_infallible_conversion() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let value = (I64 :: 42) as I8\n"))
        .expect_err_diagnostics("as must not select a fallible conversion");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation")),
        "{diagnostics:?}"
    );
}

#[test]
fn local_items_shadow_implicitly_imported_items_of_every_kind() {
    // The local type hides the prelude's `List` companion namespace too.
    let diagnostics = TypeChecker::new()
        .check(resolve("type List\nList.new ()\n"))
        .expect_err_diagnostics("a local `List` has no companion `new`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no element named `new`")),
        "{diagnostics:?}"
    );
    // A local value hides the prelude's type of the same name.
    let diagnostics = resolve_result("def List = 3\nlet values: List<I32> = List.new ()\n")
        .expect_err_diagnostics("a local `List` value hides the prelude `List` type");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `List`"),
        "{diagnostics:?}"
    );
}

#[test]
fn explicitly_imported_items_shadow_implicitly_imported_items() {
    let diagnostics = resolve_result("use std.option.Option as List\nlet values = List.new ()\n")
        .expect_err_diagnostics("an explicitly imported `List` hides the prelude `List`");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("duplicate")),
        "{diagnostics:?}"
    );
}

#[test]
fn desugared_syntax_ignores_local_items_named_after_standard_items() {
    type_check(concat!(
        "use std.result.Ok as Success\n",
        "type Convert\n",
        "type Try\n",
        "type Ok\n",
        "type Eq\n",
        "type PartialOrd\n",
        "type Bool\n",
        "type Add\n",
        "def range = 1\n",
        "def range_inclusive = 2\n",
        "def parse = (x: I32) => Result<I32, String> :: if (x > 0) { Success x } else { \"negative\" }\n",
        "def twice = (x: I32) => Result<I32, String> :: {\n",
        "    let y = parse x ?\n",
        "    parse (y + y)\n",
        "}\n",
        "let narrow = (I32 :: 42)\n",
        "let wide: I64 = narrow as I64\n",
        "let compared = (narrow == 42) && (narrow < 50) || (narrow != 3)\n",
        "let exclusive = 1 .. 3\n",
        "let inclusive = 1 ..= 3\n",
    ));
}

#[test]
fn nested_holes_in_explicit_type_arguments_infer_from_the_expected_type() {
    type_check(concat!(
        "use std.ptr.(RawPtr)\n",
        "def identity = <T> move value: T => value\n",
        "let value: Option<I32> = identity.<Option<_>> (Some 1)\n",
        "let pointer: RawPtr<I32> = (USize :: 4096) as RawPtr<_>\n",
        "let pair: (I32, String) = (I32 :: 1, \"pair\") as (I32, _)\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "use std.ptr.(RawPtr)\nlet wide: I64 = (USize :: 4096) as RawPtr<_>\n",
        ))
        .expect_err_diagnostics("a partial argument must agree with the expected type");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message
            == "compile-time argument `RawPtr<_>` conflicts with the expected `I64`"),
        "{diagnostics:?}"
    );
}

#[test]
fn imports_that_clash_with_local_definitions_name_the_conflict() {
    for source in [
        "use std.list.List\ntype List\n",
        "type List\nuse std.list.List\n",
        "use std.list.List\ndef List = 1\n",
        "use std.list.List\ntype List wrap I32\n",
    ] {
        let diagnostics = resolve_result(source).expect_err_diagnostics("the import must clash");
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["import of `List` conflicts with a definition of the same name in this module"],
            "{source}"
        );
    }
}

#[test]
fn as_syntax_consumes_move_only_sources() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "def exercise: () -> () = () => {\n",
            "let original = c_string \"owned\"\n",
            "let moved = original as CString\n",
            "let reused = original as CString\n",
            "()\n}\n",
        )))
        .expect_err_diagnostics("as must move its source like Convert.convert");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("moved")),
        "{diagnostics:?}"
    );
}

#[test]
fn standard_library_conversion_blankets_consume_non_copy_values() {
    let module = type_check(concat!(
        "type Resource wrap I32\n",
        "impl Drop Resource { cleanup = Resource value => () }\n",
        "def convert_resource: () -> Ok<Resource> | Never = () => {\n",
        "    let handle: Resource = Convert.convert (Resource 41)\n",
        "    TryConvert.try_convert handle\n",
        "}\n",
        "def identity: <T> move T -> T = <T> move value => Convert.convert value\n",
        "def attempt: <T> move T -> Ok<T> | Never = <T> move value => TryConvert.try_convert value\n",
        "let generic: Ok<Resource> | Never = attempt (identity (Resource 42))\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("identity conversion blankets should consume non-Copy values");
}

#[test]
fn standard_library_try_convert_delegates_to_convert() {
    let module = type_check(concat!(
        "use std.convert.(Convert, TryConvert)\n",
        "type Source wrap I32\n",
        "type Target wrap I32\n",
        "impl Convert Source Target { convert = move Source value => Target value }\n",
        "let converted: Target = Convert.convert (Source 41)\n",
        "let attempted: Ok<Target> | Never = TryConvert.try_convert (Source 42)\n",
        "def attempt: <From, To where Convert From To> move From -> Ok<To> | Never =\n",
        "    <From, To where Convert From To> move value => TryConvert.try_convert value\n",
        "let generic: Ok<Target> | Never = attempt (Source 43)\n",
        "let error: TryConvert.Error Source Target -> Never = value => value\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("infallible conversions should provide TryConvert with Never errors");
}

#[test]
fn standard_library_try_convert_supports_fallible_implementations() {
    let module = type_check(concat!(
        "type Source wrap I32\n",
        "type Target wrap I32\n",
        "type ConversionError wrap I32\n",
        "impl TryConvert Source Target {\n",
        "    type Error = ConversionError\n",
        "    try_convert = move Source value => match (value == 0) {\n",
        "        True() => ConversionError value,\n",
        "        False() => Ok (Target value),\n",
        "    }\n",
        "}\n",
        "let success: Ok<Target> | ConversionError = TryConvert.try_convert (Source 42)\n",
        "let failure: Ok<Target> | ConversionError = TryConvert.try_convert (Source 0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("manual fallible conversions should compile");
}

#[test]
fn every_copy_type_gets_a_blanket_clone_implementation() {
    let module = type_check(concat!(
        "let value: I32 = 41\n",
        "let cloned: I32 = Clone.clone value\n",
        "let pair: (I32, I32) = (1, 2)\n",
        "let cloned_pair: (I32, I32) = Clone.clone pair\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("the blanket `Clone` impl for `Copy` types should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn a_non_copy_type_can_implement_clone_manually() {
    let module = type_check(concat!(
        "type Resource wrap I32\n",
        "impl Drop Resource { cleanup = Resource value => () }\n",
        "impl Clone Resource { clone = Resource value => Resource value }\n",
        "def duplicate: Resource -> Resource = handle => Clone.clone handle\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a manual `Clone` implementation for a non-`Copy` type should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn rejects_an_external_clone_impl_for_a_standard_library_type() {
    let diagnostics = TypeChecker::new()
        .check(resolve("impl Clone I32 { clone = value => value }\n"))
        .expect_err_diagnostics("both `Clone` and `I32` are external");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot implement an external trait for external types")
    }));
}

#[test]
fn rejects_a_blanket_bounded_impl_that_overlaps_an_earlier_concrete_one() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Greet T { greet: T -> String }\n",
            "impl Greet I32 { greet = value => \"hi\" }\n",
            "impl<T where Copy T> Greet T { greet = value => \"hi\" }\n",
        )))
        .expect_err_diagnostics(
            "the blanket impl also covers `I32`, already implemented concretely above",
        );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));
}

#[test]
fn accepts_a_concrete_impl_beside_a_copy_blanket_impl_before_its_negative_copy() {
    // `Handle` opts out of `Copy` only after its concrete `Greet`
    // implementation, so the overlap check must see every `impl !Copy`
    // before deciding whether the blanket impl covers `Handle`.
    type_check(concat!(
        "trait Greet T { greet: T -> I32 }\n",
        "impl<T where Copy T> Greet T { greet = value => 1 }\n",
        "type Handle wrap I32\n",
        "impl Greet Handle { greet = value => 2 }\n",
        "impl !Copy Handle {}\n",
    ));
}

#[test]
fn accepts_types_that_recurse_through_shared_buffer_or_buffer() {
    type_check(concat!(
        "use std.buffer.Buffer\n",
        "use std.shared_buffer.SharedBuffer\n",
        "type SharedTree wrap (value: I32, children: SharedBuffer<SharedTree>)\n",
        "type OwnedTree wrap (value: I32, children: Buffer<OwnedTree>)\n",
        "impl !Copy OwnedTree {}\n",
    ));
}

#[test]
fn accepts_types_that_recurse_through_wrappers_storing_their_parameter_indirectly() {
    type_check(concat!(
        "use std.buffer.Buffer\n",
        "use std.list.List\n",
        "type Stack<T> wrap Buffer<T>\n",
        "type ListTree wrap (value: I32, children: List<ListTree>)\n",
        "impl !Copy ListTree {}\n",
        "type StackTree wrap (value: I32, children: Stack<StackTree>)\n",
        "impl !Copy StackTree {}\n",
    ));
}

#[test]
fn rejects_types_that_recurse_through_wrappers_storing_their_parameter_inline() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Pair<T> wrap (first: T, second: I32)\n",
            "type Loop wrap (pair: Pair<Loop>)\n",
        )))
        .expect_err_diagnostics("`Pair` stores its parameter inline, so `Loop` contains itself");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("cyclic type definition") })
    );
}

#[test]
fn rejects_constructing_a_shared_buffer_from_one_value() {
    let diagnostics = resolve_result(concat!(
        "use std.shared_buffer.SharedBuffer\n",
        "let fake: SharedBuffer<I32> = SharedBuffer 5\n",
    ))
    .expect_err_diagnostics("an opaque SharedBuffer has no constructor");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("`SharedBuffer` is not a value") })
    );
}

#[test]
fn rejects_a_blanket_impl_whose_bound_is_implemented_after_it() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl Target I32 { act = value => value }\n",
            "impl <T where Bound T> Target T { act = value => value }\n",
            "impl Bound I32 { check = value => True }\n",
        )))
        .expect_err_diagnostics(
            "`Bound I32` makes the blanket impl cover `I32` even though it is declared last",
        );
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));
}

#[test]
fn statically_checks_unused_macro_bodies_and_compile_time_helpers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        "macro unused: Expr -> Expr = _ => missing_name\n",
        concat!(
            "def unused_helper: Expr -> Expr = value => missing_name\n",
            "macro unused: Expr -> Expr = value => unused_helper value\n",
        ),
        concat!(
            "def reached_helper: I32 -> I32 = value => missing_name\n",
            "macro unused: Expr -> Expr = _ => { reached_helper 1; parse_quote { 0 } }\n",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("invalid compile-time source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("unused compile-time declarations should be checked");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("missing_name")),
            "{diagnostics:#?}",
        );
    }
}

#[test]
fn statically_checks_macro_match_coverage_before_expansion() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro invalid: Item -> Item = item => match item {\n",
                "  ModifiedItem _ => item,\n",
                "}\n",
                "macro invoked: Expr -> Expr = _ => panic \"must not expand\"\n",
                "let result = invoked (0)\n",
            )),
            root,
        )
        .expect("invalid compile-time source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("definition checking should precede expansion");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("non-exhaustive match"))
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("must not expand"))
    );
}

#[test]
fn infers_nested_compile_time_match_bindings() {
    type_check(concat!(
        "use std.syntax.(Sequence, Modifier, Ident, Type, Item)\n",
        "def accept_modifiers: Sequence<Modifier> -> () = _ => ()\n",
        "def accept_ident: Ident<String> -> () = _ => ()\n",
        "def accept_type: Type -> () = _ => ()\n",
        "def infer: Sequence<(Sequence<Modifier>, Ident<String>, Option<Type>)> -> Sequence<Item> = entries => match entries {\n",
        "  Sequence () => Sequence (),\n",
        "  Sequence (first, rest) => match first {\n",
        "    (modifiers, variant, None) => {\n",
        "      accept_modifiers modifiers; accept_ident variant; infer rest; Sequence ()\n",
        "    },\n",
        "    (modifiers, variant, Some underlying) => {\n",
        "      accept_modifiers modifiers; accept_ident variant; accept_type underlying; infer rest; Sequence ()\n",
        "    },\n",
        "  },\n",
        "}\n",
    ));
}

#[test]
fn option_marks_a_def_compile_time_only_when_it_holds_syntax() {
    // `Option` is the ordinary `std.option` type, so mentioning it does not
    // make a `def` a compile-time helper; `Option<Expr>` still does, through
    // `Expr`.
    let module = type_check(concat!(
        "use std.syntax.(Expr, parse_quote)\n",
        "def or_zero: Option<I32> -> I32 = value => match value {\n",
        "  Some number => number,\n",
        "  None => 0,\n",
        "}\n",
        "def or_default: Option<Expr> -> Expr = value => match value {\n",
        "  Some expression => expression,\n",
        "  None => parse_quote { 0 },\n",
        "}\n",
        "macro pick: Expr -> Expr = value => or_default (Some value)\n",
        "let picked: I32 = pick (or_zero (Some 7))\n",
    ));
    let names = module
        .resolved()
        .syntax()
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Binding(binding) => Some(binding.name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(names.contains(&"or_zero"), "runtime `def` was dropped: {names:?}");
    assert!(
        !names.contains(&"or_default"),
        "compile-time helper survived expansion: {names:?}"
    );
}

#[test]
fn checks_inferred_and_explicit_compile_time_match_binding_types() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (source, expected) in [
        (
            concat!(
                "use std.syntax.(Sequence, Modifier, Ident, Type, Item)\n",
                "def accept_ident: Ident<String> -> () = _ => ()\n",
                "def invalid: Sequence<(Sequence<Modifier>, Ident<String>, Option<Type>)> -> Sequence<Item> = entries => match entries {\n",
                "  Sequence () => Sequence (),\n",
                "  Sequence (first, _) => match first {\n",
                "    (modifiers, _, None) => { accept_ident modifiers; Sequence () },\n",
                "    (_, _, Some _) => Sequence (),\n",
                "  },\n",
                "}\n",
            ),
            "expected `Ident<String>`, found `Sequence<Modifier>`",
        ),
        (
            concat!(
                "use std.syntax.(Sequence, Modifier, Ident, Type, Item)\n",
                "def invalid: Sequence<(Sequence<Modifier>, Ident<String>, Option<Type>)> -> Sequence<Item> = entries => match entries {\n",
                "  Sequence () => Sequence (),\n",
                "  Sequence (first, _) => match first {\n",
                "    (modifiers: Ident<String>, _, None) => Sequence (),\n",
                "    (_, _, Some _) => Sequence (),\n",
                "  },\n",
                "}\n",
            ),
            "expected `Sequence<Modifier>`, found `Ident<String>`",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .expect("invalid compile-time binding source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid inferred binding use should fail");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn opaque_types_are_unsized_but_can_be_referenced() {
    let module = type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Token opaque\n",
        "type Handle<T> opaque\n",
        "def token: Shared<Token> -> Shared<Token> = value => value\n",
        "def handle: Shared<Handle<I32>> -> Shared<Handle<I32>> = value => value\n",
        "def any: <T where ?Sized T> Shared<T> -> Shared<T> = <T where ?Sized T> value => value\n",
        "def use_any: Shared<Token> -> Shared<Token> = value => any value\n",
    ));
    let opaque = CheckedType::Opaque {
        sized: false,
        id: staple_compiler::TypeId(usize::MAX),
        name: "Token".to_owned(),
        arguments: Vec::new(),
    };
    assert!(!opaque.is_sized());
    assert!(!module.is_copy_type(&opaque));
    assert!(CheckedType::Shared(Box::new(opaque)).is_sized());
    assert!(module.is_copy_type(&CheckedType::I32));
}

#[test]
fn rejects_opaque_types_in_sized_positions() {
    for (body, expected) in [
        ("def invalid: Token -> () = _ => ()\n", "function parameter"),
        (
            "def invalid: () -> Token = () => loop {}\n",
            "function parameters and results must be sized",
        ),
        ("type Invalid wrap Token\n", "must be sized"),
        (
            "use std.shared.(Shared)\ntype Invalid = (Token, I32)\ndef use_invalid: Shared<Invalid> -> () = _ => ()\n",
            "must be sized",
        ),
        (
            "use std.shared.(Shared)\ntype Invalid = Token | I32\ndef use_invalid: Shared<Invalid> -> () = _ => ()\n",
            "sized",
        ),
        (
            "impl Default Token { default = () => loop {} }\n",
            "implicit `Sized` bound",
        ),
        (
            "use std.shared.(Shared)\ntype Box<T> wrap T\ntype Invalid = Box<Token>\ndef use_invalid: Shared<Invalid> -> () = _ => ()\n",
            "implicit `Sized` bound",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(&format!("type Token opaque\n{body}")))
            .expect_err_diagnostics(&format!("opaque types must not be placed by value: {body}"));
        assert!(
            diagnostics.iter().any(|d| d.message.contains(expected)),
            "{body}: {diagnostics:?}"
        );
    }
}

#[test]
fn sized_opaque_modifier_is_package_visible_in_stdlib() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        "@sized_opaque\ntype Token opaque\n",
        "@std.annotation.sized_opaque\ntype Token opaque\n",
        "use std.annotation.(sized_opaque)\n@sized_opaque\ntype Token opaque\n",
        "use std.annotation.*\n@sized_opaque\ntype Token opaque\n",
        "pub use std.annotation.sized_opaque\n@sized_opaque\ntype Token opaque\n",
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .expect("source should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("user code cannot invoke sized_opaque");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("sized_opaque")),
            "{diagnostics:?}"
        );
    }
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("macro @sized_opaque: Item -> Item = item => item\n"),
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("sized_opaque name is reserved");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("reserved by the compiler")),
        "{diagnostics:?}"
    );
}

#[test]
fn recursive_constructor_modifier_is_package_visible_in_stdlib() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        "@recursive_constructor\ntype Token opaque\n",
        "@std.annotation.recursive_constructor\ntype Token opaque\n",
        "use std.annotation.(recursive_constructor)\n@recursive_constructor\ntype Token opaque\n",
        "use std.annotation.*\n@recursive_constructor\ntype Token opaque\n",
        "pub use std.annotation.recursive_constructor\n@recursive_constructor\ntype Token opaque\n",
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .expect("source should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("user code cannot invoke recursive_constructor");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("recursive_constructor")),
            "{diagnostics:?}"
        );
    }
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("macro @recursive_constructor: Item -> Item = item => item\n"),
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err_diagnostics("recursive_constructor name is reserved");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("reserved by the compiler")),
        "{diagnostics:?}"
    );
}

fn stdlib_modifier_diagnostics(path: &str, original: &str, replacement: &str) -> Vec<Diagnostic> {
    stdlib_edit_diagnostics(path, original, replacement, "")
}

/// Resolves and checks `program_source` against a copy of the standard library
/// in which the first `original` in `path` is replaced by `replacement`.
fn stdlib_edit_diagnostics(
    path: &str,
    original: &str,
    replacement: &str,
    program_source: &str,
) -> Vec<Diagnostic> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temporary = std::env::temp_dir().join(format!(
        "staple-stdlib-modifier-{}-{nonce}",
        std::process::id()
    ));
    copy_directory(&root.join("stdlib"), &temporary);
    let file = temporary.join(path);
    let source = std::fs::read_to_string(&file).unwrap();
    assert!(source.contains(original));
    std::fs::write(file, source.replacen(original, replacement, 1)).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(&temporary)
        .load_source(program_source, root)
        .expect("fixture should load");
    let result = NameResolver::new().resolve_program(program);
    std::fs::remove_dir_all(temporary).unwrap();
    match result {
        Err(diagnostics) => diagnostics,
        Ok(module) => TypeChecker::new().check(module).err().unwrap_or_default(),
    }
}

#[test]
fn sized_opaque_stdlib_contract_and_validation() {
    for (path, original, replacement, expected) in [
        (
            "std/number/types.sta",
            "@sized_opaque\npub type I32 opaque",
            "pub type I32 opaque",
            "type `I32` must be marked `@sized_opaque`",
        ),
        (
            "std/io.sta",
            "@sized_opaque\npub type IO opaque",
            "pub type IO opaque",
            "type `IO` must be marked `@sized_opaque`",
        ),
        (
            "std/number/types.sta",
            "@sized_opaque\npub type I32 opaque",
            "@sized_opaque(1)\npub type I32 opaque",
            "does not accept an argument",
        ),
        (
            "std/number/types.sta",
            "@sized_opaque\npub type I32 opaque",
            "@sized_opaque\npub type I32 = I64",
            "may only modify an opaque type declaration",
        ),
        (
            "std/sized.sta",
            "pub trait Sized T {}",
            "@sized_opaque\npub trait Sized T {}",
            "may only modify an opaque type declaration",
        ),
    ] {
        let diagnostics = stdlib_modifier_diagnostics(path, original, replacement);
        assert!(
            diagnostics.iter().any(|d| d.message.contains(expected)),
            "{diagnostics:?}"
        );
    }
    let diagnostics = stdlib_modifier_diagnostics(
        "std/number/types.sta",
        "@sized_opaque\npub type I32 opaque",
        "@std.annotation.sized_opaque\n@identity\npub type I32 opaque\nuse std.syntax.Item\nmacro @identity: Item -> Item = item => item",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn stdlib_opaque_copy_policy_uses_explicit_negative_implementations() {
    let cases = [
        ("I32", true),
        ("CChar", true),
        ("RawPtr<I32>", true),
        ("RawOpaquePtr", true),
        ("RawMutPtr<I32>", true),
        ("RawMutPtr<CString>", true),
        ("RawOpaqueMutPtr", true),
        ("IO", true),
        ("CString", false),
        ("Buffer<I32>", false),
        ("Buffer<String>", false),
        ("Reactive", false),
        ("Coroutine{}<I32>", false),
        ("Coroutine{IO}<String>", false),
        ("Task<I32>", false),
        ("Task<String>", false),
        ("Scheduler", false),
        ("Tasks", false),
        ("Wait<I32>", false),
        ("Resolver<String>", false),
        ("CompletionToken", false),
    ];
    let mut source = String::from(
        "use std.cinterop.*\nuse std.ptr.*\nuse std.coroutine.*\nuse std.io.IO\nuse std.buffer.Buffer\n",
    );
    for (index, (name, _)) in cases.iter().enumerate() {
        source.push_str(&format!("def check_copy_{index}: {name} -> () = _ => ()\n"));
    }
    let module = type_check(&source);
    for (index, (name, copy)) in cases.iter().enumerate() {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == format!("check_copy_{index}"))
            .expect("test function should exist");
        let checked = module.type_of_function(function.id).unwrap();
        assert!(checked.parameter.is_sized(), "{name}");
        assert_eq!(module.is_copy_type(&checked.parameter), *copy, "{name}");
    }
}

#[test]
fn slotted_product_aliases_normalize_function_parameters() {
    let module = type_check(concat!(
        "type Callable<Arg, Result> = Arg -> Result\n",
        "type Inputs = (I32; I32)\n",
        "type Add = Callable<Inputs, I32>\n",
        "def add: Add = (x; y) => x + y\n",
        "let answer: I32 = add 1 2\n",
        "type One = (I32;)\n",
        "def increment: (value: One;) -> I32 = (value;) => value + 1\n",
        "let plain: I32 -> I32 = increment\n",
        "let bracketed: (I32;) -> I32 = x => x + 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("aliases generate LLVM");
}

#[test]
fn slotted_products_survive_nominal_type_arguments_and_specialization() {
    let module = type_check(concat!(
        "type Inputs = (I32; I32)\n",
        "type Handler<Args> wrap (callback: Args -> I32, label: String)\n",
        "type Wrapped<Args> wrap Args -> I32\n",
        "type Tag<P> wrap I32\n",
        "def add: Inputs -> I32 = (x; y) => x + y\n",
        "def identity: Handler<Inputs> -> Handler<Inputs> = h => h\n",
        "let handler: Handler<Inputs> = Handler (callback: add, label: \"sum\")\n",
        "let result: I32 = (identity handler).callback 1 2\n",
        "let wrapped: Wrapped<Inputs> = Wrapped add\n",
        "let tag: Tag<Inputs> = Tag 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("nominal slotted products specialize");
}

#[test]
fn slotted_product_spreads_preserve_slots() {
    let module = type_check(concat!(
        "type Inputs = (I32; I32)\n",
        "type More = (...Inputs; I32)\n",
        "def add: More -> I32 = (a; b; c) => a + b + c\n",
        "let result: I32 = add 1 2 3\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("spreads compile");
}

#[test]
fn slotted_products_are_values_in_nested_and_nominal_positions() {
    let module = type_check(concat!(
        "type Inputs = (I32; I32)\n",
        "type Payload wrap Inputs\n",
        "type Nested = (Inputs, Inputs)\n",
        "type Box<T> wrap (value: T)\n",
        "let slots: Inputs = (1; 2)\n",
        "let payload: Payload = Payload 1 2\n",
        "let nested: Nested = (slots, slots)\n",
        "let boxed: Box<Inputs> = Box slots\n",
        "let sum: Inputs | I32 = slots\n",
        "let mut list: List<Inputs> = List.new ()\n",
        "list |> push slots\n",
        "def identity: Inputs -> Inputs = (a; b) => (a; b)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slotted values compile");
}

#[test]
fn slotted_generic_signatures_accept_juxtaposed_callbacks() {
    let module = type_check(concat!(
        "type Registry<A> wrap (callback: A -> (), label: String)\n",
        "def keep: <A> (Registry<A>; A -> ()) -> Registry<A> = <A> (registry; callback) => registry\n",
        "def add: (I32; I32) -> () = (a; b) => ()\n",
        "let registry: Registry<(I32; I32)> = Registry (callback: add, label: \"sum\")\n",
        "let result = keep registry add\n",
        "result.callback 1 2\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("capable generics specialize");
}

#[test]
fn slotted_generic_bodies_call_opaque_parameters_as_one_slot() {
    type_check("def apply: <A, R> (A -> R; A) -> R = <A, R> (f; value) => f value\n");
    let messages = type_check_errors(
        "def bad: <A> (A -> ()) -> () = <A> f => f 1\n",
        "a scalar cannot stand in for an opaque A",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("expected") || message.contains("cannot")),
        "{messages:?}"
    );
}

#[test]
fn slotted_generic_expected_types_and_forwarding_work() {
    let module = type_check(concat!(
        "def keep: <A> (A -> ()) -> A -> () = <A> f => f\n",
        "def forward: <A> (A -> ()) -> A -> () = <A> f => keep f\n",
        "def callback: (I32; I32) -> () = (a; b) => ()\n",
        "let explicit: (I32; I32) -> () = keep callback\n",
        "let inferred = forward callback\n",
        "explicit 1 2\n",
        "inferred 1 2\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("expected-type and forwarded generics specialize");
}

#[test]
fn slotted_generic_calls_reject_scalar_arguments_for_product_slots() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def apply: <A, R> (A -> R; A) -> R = <A, R> (f; a) => f a\n",
            "def callback: (I32; I32) -> I32 = (a; b) => a + b\n",
            "let invalid = apply callback 1\n",
        )))
        .expect_err_diagnostics("a scalar cannot fill a product-valued slot");
    assert!(
        diagnostics.iter().any(|d| d.message.contains("expected")),
        "{diagnostics:?}"
    );
}

#[test]
fn slotted_companion_members_infer_parameter_types() {
    let prefix = concat!(
        "type Handler<A> wrap (callback: A -> I32, label: String)\n",
        "companion<A> Handler<A> {\n",
        " pub def get_callback: Handler<A> -> A -> I32 = handler => handler.callback\n",
        " pub def evaluate: (Handler<A>; A) -> I32 = (handler; value) => handler.callback value\n",
        "}\n",
        "def sum: (I32; I32) -> I32 = (a; b) => a + b\n",
        "let handler: Handler<(I32; I32)> = Handler (callback: sum, label: \"sum\")\n",
    );
    let module = type_check(&format!(
        "{prefix}let callback = Handler.get_callback handler\nlet result: I32 = callback 1 2\n"
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("capable companion specializes");
    let diagnostics = TypeChecker::new()
        .check(resolve(&format!("{prefix}Handler.evaluate handler 1\n")))
        .expect_err_diagnostics("member argument must match its inferred type");
    assert!(
        diagnostics.iter().any(|d| d.message.contains("expected")),
        "{diagnostics:?}"
    );
}

#[test]
fn slotted_product_template_spreads_and_markers_normalize_after_substitution() {
    let module = type_check(concat!(
        "type Expand<Args, Result> = (...Args;) -> Result\n",
        "type Inputs = (I32; I32)\n",
        "def sum: Expand<Inputs, I32> = (a; b) => a + b\n",
        "let result: I32 = sum 1 2\n",
        "def push: (mut List<I32>; move I32) -> () = (mut list; move value) => list |> List.push value\n",
        "let mut list: List<I32> = List.new ()\n",
        "push list 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("spread templates and markers compile");
}

#[test]
fn opaque_and_phantom_parameters_accept_slotted_products() {
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "use std.ptr.RawPtr\n",
        "use std.coroutine.Task\n",
        "type Inputs = (I32; I32)\n",
        "type Handle<Args> opaque\n",
        "def pointer: RawPtr<Inputs> -> RawPtr<Inputs> = value => value\n",
        "def task: move Task<Inputs> -> Task<Inputs> = move value => value\n",
        "type Outer<P> wrap (handle: Shared<Handle<P>>)\n",
        "extern \"c\" { foreign: Shared<Handle<Inputs>> -> Outer<Inputs> }\n",
    ));
}

#[test]
fn slotted_generic_signatures_have_no_capability_restrictions() {
    type_check(concat!(
        "def constant: <A> A -> I32 = <A> _ => 1\n",
        "def takes: <A> ((A;) -> ()) -> () = <A> g => ()\n",
        "def forward: <B> (B -> ()) -> () = <B> f => takes f\n",
        "let polymorphic: <A> A -> I32 = constant\n",
    ));
    let messages = type_check_errors(
        "def invalid: <A> (A -> ()) -> () = <A> _ => { let value: A = 1; () }\n",
        "generic values still require matching types",
    );
    assert!(
        messages.iter().any(|message| message.contains("expected")),
        "{messages:?}"
    );
}

#[test]
fn single_slot_slotted_products_collapse_names_and_markers() {
    let module = type_check(concat!(
        "type One = (named: I32;)\n",
        "def identity: One -> I32 = (x;) => x\n",
        "let plain: I32 -> I32 = identity\n",
        "def append_one: (mut List<I32>;) -> () = (mut list;) => list |> push 1\n",
        "let plain_push: mut List<I32> -> () = append_one\n",
        "def take: (move String;) -> String = (move value;) => value\n",
        "let plain_take: move String -> String = take\n",
        "let mut list: List<I32> = List.new ()\n",
        "plain_push list\n",
        "let result: I32 = plain 1\n",
        "let owned: String = plain_take \"hello\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("one-slot functions compile with plain ABI");
}

#[test]
fn slotted_product_spreads_validate_after_substitution() {
    for argument in ["()", "I32"] {
        let source = format!(
            "type Expand<A> = (...A;) -> ()\ndef invalid: Expand<{argument}> = () => ()\n"
        );
        let diagnostics = TypeChecker::new()
            .check(resolve(&source))
            .expect_err_diagnostics("a spread needs a nonempty product");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("at least one slot")
                    || d.message.contains("cannot spread non-product")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn slotted_products_pass_through_generic_impl_headers() {
    let module = type_check(concat!(
        "type Handler<A> wrap (callback: A -> (), label: String)\n",
        "trait Inspect T { inspect: T -> () }\n",
        "impl<A> Inspect Handler<A> { inspect = handler => () }\n",
        "def callback: (I32; I32) -> () = (a; b) => ()\n",
        "let handler: Handler<(I32; I32)> = Handler (callback: callback, label: \"sum\")\n",
        "Inspect.inspect handler\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("a capable impl specializes its header and members together");
}

#[test]
fn slotted_products_satisfy_structural_trait_bounds() {
    type_check(concat!(
        "def bounded: <A where Copy A> (A -> ()) -> () = <A where Copy A> f => ()\n",
        "def callback: (I32; I32) -> () = (a; b) => ()\n",
        "bounded callback\n",
    ));
    let messages = type_check_errors(
        concat!(
            "def map: <T, U> (List<T>; T -> U) -> List<U> = <T, U> (list; f) => List.new ()\n",
            "def callback: (I32; I32) -> I32 = (a; b) => a + b\n",
            "let list: List<I32> = List.new ()\n",
            "map list callback\n",
        ),
        "callback parameter must match the list element type",
    );
    assert!(!messages.is_empty());
}

#[test]
fn slotted_product_aliases_cannot_own_slot_markers() {
    for source in [
        "type Invalid = (mut I32; I32)\n",
        "type Invalid = (move String)\n",
    ] {
        let error =
            staple_syntax::parse(source).expect_err("slot markers require a function parameter");
        assert!(
            error
                .message
                .contains("only allowed on a function parameter type"),
            "{error:?}"
        );
    }
}

fn type_check_errors(source: &str, reason: &str) -> Vec<String> {
    TypeChecker::new()
        .check(resolve(source))
        .expect_err_diagnostics(reason)
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

#[test]
fn from_types_introduce_their_representation_contextually() {
    let module = type_check(concat!(
        "type Meters from F64\n",
        "type Id from I64\n",
        "type Name from String\n",
        "def show: Meters -> F64 = meters => meters.*\n",
        "let literal: Meters = 1.5\n",
        "let id: Id = 7\n",
        "let name: Name = \"ada\"\n",
        "let inferred = 2.5\n",
        "let converted: Meters = inferred\n",
        "let argument = show 3.0\n",
        "let explicit = Meters inferred\n",
        "let still_f64: F64 = inferred\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("implicit `from` introductions should compile");
}

#[test]
fn from_coercion_prefers_exact_sum_alternatives_and_rejects_ambiguity() {
    type_check(concat!(
        "type B wrap I32\n",
        "type A from B\n",
        "type Other\n",
        "def exact: A | B -> I32 = value => match value {\n",
        "    a: A => 1,\n",
        "    b: B => 2,\n",
        "}\n",
        "def injected: A | Other -> I32 = value => match value {\n",
        "    a: A => 1,\n",
        "    _: Other => 2,\n",
        "}\n",
        "let first = exact (B 1)\n",
        "let second = injected (B 1)\n",
    ));
    let messages = type_check_errors(
        concat!(
            "type B wrap I32\n",
            "type A from B\n",
            "type C from B\n",
            "let ambiguous: A | C = B 1\n",
        ),
        "two `from B` alternatives are ambiguous",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("can be injected into more than one alternative")),
        "{messages:?}"
    );
}

#[test]
fn from_coercion_applies_once_to_the_whole_expression() {
    let messages = type_check_errors(
        concat!(
            "use std.shared.(Shared)\n",
            "type B wrap I32\n",
            "type A from B\n",
            "type C from A\n",
            "let chained: C = B 1\n",
            "let pair = (B 1, B 2)\n",
            "let lifted_pair: (A, A) = pair\n",
            "let reference = Shared (B 1)\n",
            "let lifted_reference: Shared<A> = reference\n",
            "def make: () -> B = () => B 1\n",
            "let lifted_function: () -> A = make\n",
        ),
        "`from` coercions neither chain nor lift",
    );
    for expected in [
        "expected `C`, found `B`",
        "expected `(A, A)`, found `(B, B)`",
        "expected `Shared<A>`, found `Shared<B>`",
        "expected `() -> A`, found `() -> B`",
    ] {
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "missing `{expected}` in {messages:?}"
        );
    }
}

#[test]
fn from_types_are_not_subtypes_and_do_not_share_traits_or_members() {
    let messages = type_check_errors(
        concat!(
            "type B wrap I32\n",
            "type A from B\n",
            "trait Named T { name: T -> I32 }\n",
            "impl Named A { name = value => 1 }\n",
            "companion A { pub def legs: A -> I32 = value => 4 }\n",
            "def named: <T where Named T> T -> I32 = <T where Named T> value => Named.name value\n",
            "def bounded: <T where T <: A> T -> () = <T where T <: A> value => ()\n",
            "let by_trait = named (B 1)\n",
            "let by_bound = bounded (B 1)\n",
            "let by_member = (B 1) |> legs\n",
        ),
        "`from` introductions stay out of traits, bounds, and members",
    );
    for expected in [
        "trait bound is not satisfied for",
        "is not a subtype of",
        "no compatible local function, accessible companion, or visible trait method",
    ] {
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "missing `{expected}` in {messages:?}"
        );
    }
}

#[test]
fn implicit_from_introduction_moves_its_source() {
    let messages = type_check_errors(
        concat!(
            "type Resource wrap (I32, String)\n",
            "impl !Copy Resource {}\n",
            "type Owner from Resource\n",
            "def own: Owner -> I32 = owner => 1\n",
            "def inspect: Resource -> I32 = value => 2\n",
            "def run = () => {\n",
            "    let value = Resource (1, \"a\")\n",
            "    let first = own value\n",
            "    let second = inspect value\n",
            "    second\n",
            "}\n",
        ),
        "an implicit introduction consumes a non-Copy source",
    );
    assert!(
        messages
            .iter()
            .any(|message| message == "use of moved value"),
        "{messages:?}"
    );
}

#[test]
fn patterns_see_through_one_from_layer() {
    let module = type_check(concat!(
        "type Choice from I32 | String\n",
        "def choose: Choice -> I32 = choice => match choice {\n",
        "    number: I32 => number,\n",
        "    \"one\" => 1,\n",
        "    _: String => 0,\n",
        "}\n",
        "def flag: Bool | I32 -> I32 = value => match value {\n",
        "    True => 1,\n",
        "    False => 0,\n",
        "    number: I32 => number,\n",
        "}\n",
        "def nested: Option<Bool> -> I32 = value => match value {\n",
        "    Some True => 1,\n",
        "    Some False => 2,\n",
        "    None => 0,\n",
        "}\n",
        "def propagate: Option<I32> -> Option<I32> = value => {\n",
        "    let ?Some(number) = value\n",
        "    Some (number + 1)\n",
        "}\n",
        "type Point from (I32, I32)\n",
        "def sum: Point -> I32 = point => {\n",
        "    let (x, y) = point\n",
        "    x + y\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("see-through patterns should compile");
    let messages = type_check_errors(
        concat!(
            "def incomplete: Bool | I32 -> I32 = value => match value {\n",
            "    True => 1,\n",
            "    number: I32 => number,\n",
            "}\n",
        ),
        "seeing through `Bool` still checks exhaustiveness",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("non-exhaustive match")),
        "{messages:?}"
    );
    // Only one `from` layer is seen through.
    let messages = type_check_errors(
        concat!(
            "type Choice from Bool | I32\n",
            "def choose: Choice -> I32 = choice => match choice {\n",
            "    True => 1,\n",
            "    _ => 0,\n",
            "}\n",
        ),
        "patterns see through a single `from` layer",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("singleton pattern `True` cannot match")),
        "{messages:?}"
    );
}

#[test]
fn named_types_are_indexed_only_through_their_own_implementations() {
    for (source, reason) in [
        (
            "type Triple wrap (I32, I32, I32)\nlet first = (Triple (1, 2, 3))[0]\n",
            "a `wrap` type does not index its representation implicitly",
        ),
        (
            "type Names from (String, String)\nlet names: Names = (\"ada\", \"grace\")\nlet name = names[1]\n",
            "a `from` type does not index its representation implicitly",
        ),
    ] {
        let messages = type_check_errors(source, reason);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("no `Index` implementation is available")),
            "{reason}: {messages:?}"
        );
    }
    let messages = type_check_errors(
        concat!(
            "type Triple wrap (I32, I32, I32)\n",
            "def run = () => {\n",
            "    let mut triple = Triple (1, 2, 3)\n",
            "    triple[1] = 20\n",
            "}\n",
        ),
        "a wrapper is not index-assignable without `MutateIndex`",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("requires a `MutateIndex` implementation")),
        "{messages:?}"
    );

    // Explicit representation access and user implementations still index.
    let module = type_check(concat!(
        "type Triple wrap (I32, I32, I32)\n",
        "type Cells wrap (I32, I32)\n",
        "impl Index Cells USize { type Output = I32; index = (Cells cells, position) => cells[position] }\n",
        "def run = () => {\n",
        "    let mut triple = Triple (1, 2, 3)\n",
        "    triple.*[1] = 20\n",
        "    let second: I32 = triple.*[1]\n",
        "    let cell: I32 = (Cells (4, 5))[1]\n",
        "    second + cell\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("explicit representation indexing and user `Index` should compile");
}
#[test]
fn from_types_reject_recursive_constructors() {
    let diagnostics = stdlib_modifier_diagnostics(
        "std/shared.sta",
        "pub type Shared<T where ?Sized T> wrap pub T",
        "pub type Shared<T where ?Sized T> from pub T",
    );
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("`@recursive_constructor` types must use `wrap`, not `from`")),
        "{diagnostics:?}"
    );
}

#[test]
fn recursive_constructor_stdlib_contract_and_validation() {
    for (path, original, replacement, expected) in [
        (
            "std/shared.sta",
            "@recursive_constructor\npub type Shared",
            "@recursive_constructor(1)\npub type Shared",
            "`@recursive_constructor` does not accept an argument",
        ),
        (
            "std/shared.sta",
            "@recursive_constructor\npub type Shared",
            "pub type Shared",
            "type `Shared` must be marked `@recursive_constructor`",
        ),
        (
            "std/shared.sta",
            "@recursive_constructor\npub type Shared",
            "@recursive_constructor\nlet value = 1\npub type Shared",
            "`@recursive_constructor` may only modify a type declaration",
        ),
        (
            "std/annotation.sta",
            "macro @recursive_constructor: Item -> Item",
            "macro @recursive_constructor: Item -> Item = item => item",
            "compiler-provided modifier `@recursive_constructor` must be bodyless",
        ),
    ] {
        let diagnostics = stdlib_modifier_diagnostics(path, original, replacement);
        assert!(
            diagnostics.iter().any(|d| d.message.contains(expected)),
            "{diagnostics:?}"
        );
    }
    let diagnostics = stdlib_modifier_diagnostics(
        "std/shared.sta",
        "@recursive_constructor\npub type Shared",
        "@std.annotation.recursive_constructor\npub type Shared",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn intrinsic_values_require_a_direct_call() {
    for source in [
        "let read = snapshot\n",
        "let read: I32 -> I32 = snapshot\n",
        "use std.coroutine.spawn\nlet start = spawn\n",
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("intrinsic values must fail before codegen");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("cannot be used as a value; call it directly")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn generic_c_extern_bindings_are_rejected() {
    let diagnostics = TypeChecker::new()
        .check(resolve("extern \"c\" { identity: <T> T -> T }\n"))
        .expect_err_diagnostics("C extern generics are unsupported");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message
            == "external bindings cannot have compile-time parameters")
    );
}

#[test]
fn reserved_intrinsic_abi_is_rejected_in_nested_user_modules() {
    for source in [
        "extern \"staple-intrinsic\" { drop: <T> move T -> () }\n",
        "mod nested { extern \"staple-intrinsic\" { drop: <T> move T -> () } }\n",
        "type Box<T> wrap T\ncompanion<T> Box<T> { extern \"staple-intrinsic\" { replace: Box<T> -> T } }\n",
    ] {
        let diagnostics =
            resolve_result(source).expect_err_diagnostics("intrinsic ABI is reserved");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.message
                == "the `staple-intrinsic` ABI is reserved for the standard library"),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn intrinsic_registration_checks_names_and_loaded_modules() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (relative, before, after, expected) in [
        (
            "std/buffer.sta",
            "__buffer_length: <T>",
            "__unknown: <T>",
            "unknown Staple intrinsic `__unknown`",
        ),
        (
            "std/shared.sta",
            "    __shared_replace: <T> (mut Shared<T>; move T) -> T\n",
            "",
            "standard library `std.shared` does not declare intrinsic `__shared_replace`",
        ),
        (
            "std/coroutine.sta",
            "    spawn: <T, effect E> Coroutine{E}<T> ->{Tasks, E} Task<T>",
            "",
            "standard library `std.coroutine` does not declare intrinsic `spawn`",
        ),
    ] {
        let path = root.join("stdlib").join(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap()
            .replace(before, after);
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source_at(&path, &source)
            .expect("stdlib overlay should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err_diagnostics("invalid intrinsic declaration should fail");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "{diagnostics:?}"
        );
    }
    let resolved = resolve("let value = 1\n");
    assert!(
        !resolved
            .program()
            .modules()
            .iter()
            .any(|module| module.path.ends_with("std/coroutine.sta"))
    );
}

#[test]
fn wrapped_intrinsics_are_registered_once() {
    use staple_compiler::IntrinsicFunction;
    let resolved = resolve("use std.coroutine\n");
    let intrinsics = resolved.intrinsic_functions();
    for expected in [
        IntrinsicFunction::TaskCancel,
        IntrinsicFunction::ResolverCancel,
        IntrinsicFunction::CompletionTokenCancel,
        IntrinsicFunction::SharedReplace,
        IntrinsicFunction::SharedBufferLength,
    ] {
        assert_eq!(
            intrinsics
                .values()
                .filter(|intrinsic| **intrinsic == expected)
                .count(),
            1
        );
    }
}

#[test]
fn generic_intrinsic_trait_bounds_are_checked_at_calls() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let path = root.join("stdlib/std/reactive.sta");
    let source = std::fs::read_to_string(&path)
        .unwrap()
        .replace("pub mod\n", "pub mod\n\nuse std.cmp.Eq\n")
        .replace("snapshot: <T>", "snapshot: <T where Eq T>")
        + "\nlet _ = snapshot (() => ())\n";
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source_at(&path, &source)
        .expect("generic intrinsic overlay should load");
    let resolved = NameResolver::new()
        .resolve_program(program)
        .expect("generic bound should resolve");
    let diagnostics = TypeChecker::new()
        .check(resolved)
        .expect_err_diagnostics("intrinsic bounds must be enforced");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("trait bound is not satisfied")),
        "{diagnostics:?}"
    );
}

#[test]
fn drop_cleanup_cannot_be_referenced_directly() {
    let prelude = concat!(
        "type Resource wrap I32\n",
        "impl Drop Resource { cleanup = Resource value => () }\n",
    );
    for usage in [
        "def go = () => { let a = Resource 1; Drop.cleanup a }\n",
        "def go = () => { let a = Resource 1; cleanup a }\n",
        "def go = () => { let a = Resource 1; Drop.cleanup (a) }\n",
        "let release: Resource -> () = Drop.cleanup\n",
    ] {
        let source = format!("{prelude}{usage}");
        let diagnostics = TypeChecker::new()
            .check(resolve(&source))
            .expect_err_diagnostics("direct cleanup must be rejected");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .starts_with("`Drop.cleanup` runs automatically")),
            "{usage}: {diagnostics:?}"
        );
    }
    type_check(&format!(
        "{prelude}def go = () => {{ let a = Resource 1; drop a }}\n"
    ));
}

#[test]
fn fixed_arrays_do_not_convert_to_shared_buffers_implicitly() {
    for source in [
        "use std.shared_buffer.SharedBuffer\nlet fixed: (I32 ^ 2) = (1, 2)\nlet values: SharedBuffer<I32> = fixed\n",
        "use std.shared_buffer.SharedBuffer\nlet values: SharedBuffer<I32> = 8\n",
        "use std.shared_buffer.SharedBuffer\nlet fixed: (I32 ^ 2) = (1, 2)\nlet count = SharedBuffer.length fixed\n",
        "use std.shared_buffer.SharedBuffer\ndef coerce: <T, N where Natural N> (T ^ N) -> SharedBuffer<T> = <T, N where Natural N> value => value\n",
    ] {
        TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("an array must be converted with `SharedBuffer.from_array`");
    }
    // The explicit conversion infers the element type for generic callers.
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "use std.shared_buffer.SharedBuffer\n",
        "let fixed: (I32 ^ 3) = (1, 2, 3)\n",
        "let first: Shared<I32> = SharedBuffer.get_shared_unchecked (SharedBuffer.from_array fixed) 0\n",
        "let count: USize = SharedBuffer.length (SharedBuffer.from_array fixed)\n",
    ));
}

#[test]
fn call_site_type_arguments_specialize_functions_and_constructors() {
    let module = type_check(concat!(
        "use std.list.List\n",
        "def identity: <T> move T -> T = <T> move x => x\n",
        "def second: <A, B> (move B; move A) -> B = <A, B> (move b; move a) => b\n",
        "let list = List.new.<I32> ()\n",
        "let empty: () -> List<I32> = List.new.<I32>\n",
        "let another = empty ()\n",
        "let value = identity.<I32> 42\n",
        "let text = identity.<String> \"hello\"\n",
        "let result = second.<String, I32> 7 \"discarded\"\n",
        "let inferred = second.<_, I32> 8 \"inferred\"\n",
        "type Box<T> wrap T\n",
        "let wrapped = Box.<I32> 9\n",
        "type Bounded<T where T <: I32> wrap T\n",
        "let inferred_wrapper = Bounded.<_> 10\n",
        "let wide = identity.<I64> 42\n",
    ));
    let context = Context::create();
    let lowered = lower(&module);
    CodeGenerator::new(&context)
        .compile_root(&lowered)
        .expect("explicit generic arguments should lower and generate code");
    assert!(lowered.planned_instance_names("identity").len() >= 2);
}

#[test]
fn call_site_type_arguments_reject_wrong_arity_and_conflicts() {
    for (source, message) in [
        (
            "def f: <T> move T -> T = <T> move x => x\nlet x = f.<I32, String> 1\n",
            "expects 1 type arguments",
        ),
        (
            "def f: I32 -> I32 = x => x\nlet x = f.<I32> 1\n",
            "call-site type arguments require a generic function or constructor",
        ),
        (
            "def f: <T> move T -> T = <T> move x => x\nlet x = f.<I32> \"wrong\"\n",
            "I32",
        ),
        (
            "def f: <T> move T -> T = <T> move x => x\nlet x: String = f.<I32> 1\n",
            "String",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("explicit arguments should be checked");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(message)),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn call_site_type_arguments_enforce_trait_bounds() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Marker T { marker: T -> Bool }\n",
            "def f: <T where Marker T> move T -> T = <T where Marker T> move x => x\n",
            "let specialized = f.<I32>\n",
        )))
        .expect_err_diagnostics("explicit specialization must enforce bounds");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("trait bound")),
        "{diagnostics:?}"
    );
}

#[test]
fn call_site_type_arguments_work_in_generic_bodies_and_macro_quotes() {
    let module = type_check(concat!(
        "use std.cmp.Eq\n",
        "def identity: <T> move T -> T = <T> move x => x\n",
        "def relay: <T> move T -> T = <T> move x => identity.<T> x\n",
        "let value = relay.<I32> 4\n",
        "macro make_list: Type -> Expr = T => parse_quote { std.list.List.new.<$T> () }\n",
        "let list = make_list I32\n",
        "let same = Eq.eq 1 1\n",
        "let equals: (I32; I32) -> Bool = Eq.eq\n",
        "let same_again = equals 2 2\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("explicit arguments should survive generic and macro expansion");
}

#[test]
fn call_site_type_arguments_enforce_sized_and_subtype_bounds() {
    for (source, message) in [
        (
            "use std.shared.(Shared)\ntype Token opaque\ndef f: <T> Shared<T> -> Shared<T> = <T> x => x\nlet specialized = f.<Token>\n",
            "Sized",
        ),
        (
            "def f: <T where T <: I32> T -> T = <T where T <: I32> x => x\nlet specialized = f.<String>\n",
            "subtype",
        ),
        (
            "def f: <T> move T -> T = <T> move x => x\nlet specialized = f.<I32>.<String>\n",
            "one call-site type argument list",
        ),
        (
            "type Box<T where T <: String> wrap T\nlet specialized = Box.<I32>\n",
            "subtype",
        ),
        (
            "type Box<T where T <: String> wrap T\nlet specialized = Box.<_> 1\n",
            "subtype",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics(source);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(message)),
            "{diagnostics:?}"
        );
    }
    let module = type_check(
        "use std.shared.(Shared)\ntype Token opaque\ndef f: <T where ?Sized T> Shared<T> -> Shared<T> = <T where ?Sized T> x => x\nlet specialized = f.<Token>\n",
    );
    lower(&module);
}

#[test]
fn forall_placement_resolves_with_scoped_parameters() {
    for source in [
        "def f: (<T> T -> T) -> I32\n",
        "def f: I32 -> (<T> T -> T)\n",
        "type Box<T> wrap T\ndef f: Box<<T> T -> T>\n",
        "type Alias = <T> T -> T\n",
    ] {
        resolve_result(source).unwrap_or_else(|diagnostics| panic!("{source}: {diagnostics:?}"));
    }
    resolve_result("trait Test T { apply: <U> (T, U) -> U }\n")
        .expect("trait member forall resolves");
}

#[test]
fn forall_preserves_companion_parameter_order_and_function_annotations() {
    let resolved = resolve(
        "type Box<T> wrap T\ncompanion<T> Box<T> { pub def choose: <U> (move Box<T>, move U) -> U = <U> (move Box x, move y) => y }\nlet result: String = Box.choose.<I32, String> (Box 1, \"yes\")\n",
    );
    let function = resolved
        .functions()
        .iter()
        .find(|f| f.name.contains("choose"))
        .unwrap();
    assert_eq!(
        function
            .type_parameters
            .iter()
            .flat_map(|p| p.names())
            .collect::<Vec<_>>(),
        ["T", "U"]
    );
    assert!(matches!(
        &function.binding_annotation,
        Some(Type::Forall(_))
    ));
    TypeChecker::new()
        .check(resolved)
        .expect("explicit arguments keep merged order");
}

#[test]
fn forall_generic_binding_macro_expansion_remains_parseable_and_hygienic() {
    let source = concat!(
        "macro emit: Expr -> Item = ignored => parse_quote { def identity: <T> move T -> T = <T> move value => value }\n",
        "emit 1\n",
        "let result: I32 = identity 42\n",
    );
    let resolved = resolve(source);
    let expanded = staple_compiler::render_expanded_module(resolved.program()).unwrap();
    let compact = expanded.split_whitespace().collect::<String>();
    assert!(compact.contains("identity:<T>"), "{expanded}");
    assert!(!compact.contains("identity<T>"), "{expanded}");
    let reparsed = parse(&expanded).unwrap();
    assert!(
        reparsed
            .items
            .iter()
            .any(|item| matches!(item, Item::Binding(binding) if binding.generics().is_some()))
    );
    TypeChecker::new()
        .check(resolved)
        .expect("fresh generic annotation must type-check");
}

#[test]
fn forall_preserves_generic_compile_time_helpers() {
    type_check(concat!(
        "def identity: <T> Expr -> Expr = <T> value => value\n",
        "macro relay: Expr -> Expr = value => identity value\n",
        "let answer: I32 = relay 42\n",
        "def concrete_type: <T> () -> Type = <T> () => parse_quote { I32 }\n",
        "macro typed: Expr -> Expr = value => { let ty: Type = concrete_type (); parse_quote { $ty :: $value } }\n",
        "let typed_answer: I32 = typed 7\n",
    ));
}

#[test]
fn checked_schemes_preserve_enclosing_rigid_parameters() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def outer: <T> (move T) -> I32 = <T> move value => {\n",
            "  def helper: (move T) -> T = move x => x\n",
            "  helper 5\n",
            "  0\n",
            "}\n",
            "outer 1\n",
        )))
        .expect_err_diagnostics("a local nongeneric helper cannot instantiate its enclosing T");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `T`, found `I32`")),
        "{diagnostics:?}"
    );
}

#[test]
fn checked_schemes_instantiate_local_generics_independently() {
    type_check(concat!(
        "def outer: <T> (move T) -> T = <T> move value => {\n",
        "  def helper: <U> (move U) -> U = <U> move x => x\n",
        "  helper value\n",
        "}\n",
        "outer 1\n",
    ));
}

#[test]
fn checked_schemes_expose_templates_and_explicit_effect_arguments() {
    let module = type_check(concat!(
        "def apply: <effect E> (() ->{E} I32) ->{E} I32 = <effect E> f => f ()\n",
        "let value: I32 = apply.<(_ -> _)> (() => 42)\n",
    ));
    let binding = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == "apply" => Some(binding),
            _ => None,
        })
        .expect("effect-generic binding");
    let symbol = module.resolved().symbol_for(binding.syntax.id).unwrap();
    let scheme = module
        .scheme_of_symbol(symbol)
        .expect("effect parameter has a scheme");
    assert!(matches!(
        scheme.parameters.as_slice(),
        [staple_compiler::CheckedSchemeParameter::Effect { .. }]
    ));
    assert_eq!(module.type_of_symbol(symbol), Some(scheme.body.as_ref()));
    assert_eq!(
        module.declared_type_of_symbol(symbol),
        Some(scheme.body.as_ref().clone())
    );
}

#[test]
fn checked_schemes_preserve_concrete_constructor_residuals() {
    type_check(concat!(
        "use std.shared_buffer.SharedBuffer\n",
        "let empty: SharedBuffer<I32> = SharedBuffer.from_array ()\n",
    ));
}

#[test]
fn checked_schemes_preserve_invalid_generic_body_diagnostics() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def bad: <T> I32 -> I32 = 42\n"))
        .expect_err_diagnostics("a generic def still requires a function body");
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "expected `I32 -> I32`, found `I32`",
            "a generic `def` requires a function body",
        ]
    );
}

#[test]
fn checked_schemes_preserve_trait_argument_inference_from_expected_types() {
    type_check(concat!(
        "use std.cmp.Eq\n",
        "let equals: (I32; I32) -> Bool = Eq.eq.<_>\n",
        "let same: Bool = equals 1 1\n",
    ));
}

#[test]
fn polymorphic_value_examples_check() {
    for source in [
        "def identity: <T> move T -> T = <T> move value => value\nlet id = identity\n",
        "def identity: <T> move T -> T = <T> move value => value\nlet annotated: <U> move U -> U = identity\n",
        "def identity: <T> move T -> T = <T> move value => value\nlet concrete: I32 -> I32 = identity\n",
        "type Functions wrap (identity: <T> move T -> T,)\n",
        "def consume: (<T> move T -> T) -> (I32, String) = f => (f 42, f \"hello\")\n",
        "def identity: <T> move T -> T = <T> move value => value\nlet mut selected: <U> move U -> U = identity\nselected = identity\n",
        "let id: <T> move T -> T = <T> move value => value\nlet a = id 42\nlet b = id \"hello\"\n",
    ] {
        TypeChecker::new()
            .check(resolve(source))
            .unwrap_or_else(|diagnostics| panic!("{source}: {diagnostics:?}"));
    }
}

#[test]
fn polymorphic_values_check_in_every_value_position() {
    const ID: &str = "def identity: <T> move T -> T = <T> move value => value\n";
    for source in [
        "let id = identity\nlet a = id 42\nlet b = id \"hello\"\n",
        "let id = identity\nlet a = id.<_> 42\n",
        "let id = (<U> move U -> U) :: identity\nlet a = id.<I32> 42\n",
        "let id = (<U> move U -> U) :: (<U> move x => x)\nlet a = id 42\n",
        "def consume: (<U> move U -> U) -> I32 = f => f 42\nlet a = consume (<U> move x => x)\n",
        "def consume: (<U where Copy U> U -> U) -> I32 = f => f 42\nlet a = consume (<U where Copy U> x => x)\n",
        "let f: <U where Copy U> U -> U = <U where Copy U> x => x\nlet a = f 42\n",
        "def consume: (<effect E> (() ->{E} I32) ->{E} I32) -> I32 = f => f (() => 42)\nlet a = consume (<effect E> f => f ())\n",
        "def outer: <A where Copy A> A -> I32 = <A where Copy A> a => { let f: <U where Copy U> U -> U = <U where Copy U> x => { let saved = a; x }; f 42 }\n",
        "def consume: (<U> move U -> U) -> (I32, String) = f => (f 42, f \"hello\")\nlet a = consume identity\n",
        "type Functions wrap (identity: <U> move U -> U,)\nlet fs = Functions (identity: identity)\nlet a = fs.identity 42\n",
        "def make: () -> (<U> move U -> U) = () => identity\nlet a = (make ()).<I32> 42\nlet b = (make ()).<_> 42\n",
        "let mut selected: <U> move U -> U = identity\nselected = (<V> move V -> V) :: identity\nlet a = selected 42\n",
        "type Box<T> wrap T\nlet b: Box<<U> move U -> U> = Box identity\n",
        "type Box<T> wrap T\nlet b = Box.<(<U> move U -> U)> identity\n",
        "let id: <T> move T -> T = <T> move (x: T) => x\nlet a = id 42\n",
        "def apply: <effect E> (() ->{E} I32) ->{E} I32 = <effect E> f => f ()\nlet f = apply\nlet a: I32 = f (() => 42)\n",
        "let apply: <effect E> (() ->{E} I32) ->{E} I32 = <effect E> f => f ()\nlet a: I32 = apply (() => 42)\n",
        "def copied: <T where Copy T> T -> T = <T where Copy T> x => x\nlet c: <U where Copy U> U -> U = copied\nlet a = c 42\n",
        "def copied: <T where Copy T> T -> T = <T where Copy T> x => x\nlet c = copied\nlet a = c.<I32> 42\n",
        "def apply: <effect E> (() ->{E} I32) ->{E} I32 = <effect E> f => f ()\nlet alias: <effect F> (() ->{F} I32) ->{F} I32 = apply\nlet a = alias (() => 42)\n",
        "let pair: (a: <T> move T -> T, b: <U> move U -> U) = (a: identity, b: identity)\n",
    ] {
        let source = format!("{ID}{source}");
        TypeChecker::new()
            .check(resolve(&source))
            .unwrap_or_else(|diagnostics| panic!("{source}: {diagnostics:?}"));
    }
}

#[test]
fn rejects_invalid_polymorphic_values() {
    for (source, message) in [
        (
            "trait Test T { apply: (<U> move U -> U) -> I32 }\n",
            "must mention trait parameter `T`",
        ),
        ("let f: <T> T = 42\n", "function body"),
        (
            "type Token wrap I32\nimpl !Copy Token {}\ndef escaping: Token -> (<U> move U -> U) = token => (<U> move value => { let n = token.*; value })\nlet token = Token 42\nlet f = escaping token\n",
            "borrowed closure",
        ),
        (
            "def copied: <T where Copy T> T -> T = <T where Copy T> x => x\ndef make: () -> (<U where Copy U> U -> U) = () => copied\ntype Token wrap I32\nimpl !Copy Token {}\nlet f = (make ()).<Token>\n",
            "trait bound",
        ),
        (
            "let f: <effect E> () ->{E} () = <effect E> () => std.io.print \"hello\"\n",
            "declared effect set",
        ),
        (
            "def copied: <T where Copy T> T -> T = <T where Copy T> x => x\ntype Token wrap I32\nimpl !Copy Token {}\nlet f = copied\nlet result = f (Token 42)\n",
            "trait bound",
        ),
        (
            "def id: <T> move T -> T = <T> move x => x\nlet f = id\nlet bad = f.<I32, String> 42\n",
            "type arguments",
        ),
        (
            "use std.io.IO\ndef apply: <effect E> (() ->{E} I32) ->{E} I32 = <effect E> f => f ()\nlet bad: (() ->{IO} I32) -> I32 = apply\n",
            "expected",
        ),
        ("let f: <T> move T -> T = <T> move x => 42\n", "expected"),
        (
            "def mono: I32 -> I32 = x => x\nlet f: <T> T -> T = mono\n",
            "expected",
        ),
        (
            "def id: <T> move T -> T = <T> move x => x\nlet mut f = id\nf = (x: I32) => x\n",
            "expected",
        ),
        (
            "def copied: <T where Copy T> T -> T = <T where Copy T> x => x\nlet f: <U> U -> U = copied\n",
            "trait bound",
        ),
        (
            "def id: <T> move T -> T = <T> move x => x\ntype Box<T> wrap T\nlet b = Box id\n",
            "concrete expected type",
        ),
        (
            "def id: <T> move T -> T = <T> move x => x\nlet signal f = id\n",
            "signal bindings",
        ),
        (
            "extern \"C\" { f: (<T> T -> T) -> I32 }\n",
            "external signatures",
        ),
        (
            "def consume: ((<T> T -> T) -> I32) -> I32 = f => 42\ndef mono: (I32 -> I32) -> I32 = f => 42\nlet a = consume mono\n",
            "expected",
        ),
        ("let f = x => x\nlet g: <T> T -> T = f\n", "expected"),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics(source);
        assert!(
            diagnostics.iter().any(|d| d.message.contains(message)),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn lowers_instantiation_and_subsumption() {
    let module = type_check(concat!(
        "def id: <T> move T -> T = <T> move x => x\n",
        "def first: <A, B> (move A, move B) -> A = <A, B> (move a, move b) => a\n",
        "let f = id\n",
        "let c: move I32 -> I32 = f\n",
        "let explicit = f.<I32>\n",
        "let swapped: <Y, X> (move X, move Y) -> X = first\n",
        "let pairs: <T> move (T, T) -> (T, T) = id\n",
    ));
    lower(&module);
}

#[test]
fn instantiation_converts_function_typed_arguments() {
    // The adapter converts the concrete callback into the scheme body's
    // dynamic shape; the corpus program
    // `polymorphic_function_typed_instantiation` runs it.
    let module = type_check(concat!(
        "def twice: <T where Copy T> (T -> T, T) -> T = <T where Copy T> (f, x) => f (f x)\n",
        "let stored = twice\n",
        "let concrete: (I32 -> I32, I32) -> I32 = stored\n",
    ));
    lower(&module);
}

#[test]
fn instantiation_converts_values_mixing_type_parameters_with_functions() {
    // One argument's instantiated type mixes the surrounding run-time
    // parameter `A` with a concrete callback; its conversion reads the layout
    // from a descriptor. The corpus program
    // `polymorphic_mixed_layout_instantiation` runs this shape.
    let module = type_check(concat!(
        "let outer: <A where Copy A> A -> A = <A where Copy A> a => {\n",
        "    let poly: <T, U where Copy T, Copy U> ((T -> T, U), I32) -> U = <T, U where Copy T, Copy U> (pair, n) => { let (f, u) = pair; u }\n",
        "    let mixed: ((I32 -> I32, A), I32) -> A = poly\n",
        "    mixed ((x => x, a), 1)\n",
        "}\n",
    ));
    lower(&module);
}

/// The 1-based line of a diagnostic's span.
fn diagnostic_line(diagnostic: &staple_syntax::Diagnostic) -> usize {
    match &diagnostic.span {
        staple_syntax::Span::User {
            location: Some(location),
            ..
        } => location.line,
        span => panic!("diagnostic has no source line: {span:?}"),
    }
}

#[test]
fn reports_each_polymorphic_value_restriction_at_its_construction() {
    for (source, message) in [
        (
            concat!(
                "use std.coroutine.*\n",
                "def launch: <T> move T -> Coroutine{}<I32> = <T> move value => coro { 1 }\n",
                "let stored = launch\n",
            ),
            "functions with coroutine bodies cannot be used as polymorphic values yet",
        ),
        (
            concat!(
                "def repeat: <T, N where Copy T, Natural N> T -> N -> (T ^ N) = <T, N where Copy T, Natural N> value => n => (value ^ N)\n",
                "let stored = repeat\n",
            ),
            "array lengths cannot depend on a polymorphic value's type parameters yet",
        ),
        (
            concat!(
                "use std.shared.(Shared)\n",
                "def read: <T> Shared<T -> T> -> I32 = <T> r => 1\n",
                "let stored = read\n",
            ),
            "function values inside references cannot be passed to a polymorphic value's implementation",
        ),
    ] {
        let module = type_check(source);
        let diagnostics = Lowerer::new()
            .lower(&module)
            .expect_err_diagnostics("a restricted function cannot become a polymorphic value");
        let line = source.lines().count();
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message == message)
            .unwrap_or_else(|| panic!("{message}: {diagnostics:?}"));
        assert_eq!(
            diagnostic_line(diagnostic),
            line,
            "{message} must point at the construction site"
        );
    }
}

#[test]
fn bounded_functions_become_polymorphic_values() {
    // Bounds become hidden dictionaries, so the value constructs and a call
    // through it passes the dictionary for the instantiation.
    lower(&type_check(concat!(
        "trait Named T { name: T -> String }\n",
        "impl Named I32 { name = value => \"int\" }\n",
        "def label: <T where Named T> T -> String = <T where Named T> value => Named.name value\n",
        "let stored = label\n",
        "let answer = stored 3\n",
    )));
}

#[test]
fn structural_dictionaries_cover_index_mutation_and_iteration() {
    // Indexing, indexed assignment and `for` over tuple shapes of an
    // abstract element type get structural dictionaries.
    lower(&type_check(concat!(
        "def first: <T where Copy T> (T, T) -> T = <T where Copy T> pair => pair[USize :: 0]\n",
        "let stored_first: <T where Copy T> (T, T) -> T = first\n",
        "def put: <T where Copy T> (mut (T, T), T) -> () = <T where Copy T> (mut pair, value) => { pair[USize :: 1] = value }\n",
        "let stored_put: <T where Copy T> (mut (T, T), T) -> () = put\n",
        "def each: <T where Copy T> (T, T, T) -> () = <T where Copy T> triple => { for item in triple { } }\n",
        "let stored_each: <T where Copy T> (T, T, T) -> () = each\n",
    )));
}

#[test]
fn restricted_functions_work_when_called_directly() {
    for source in [
        concat!(
            "trait Named T { name: T -> String }\n",
            "impl Named I32 { name = value => \"int\" }\n",
            "def label: <T where Named T> T -> String = <T where Named T> value => Named.name value\n",
            "let direct = label 3\n",
        ),
        concat!(
            "def first: <T where Copy T> (T, T) -> T = <T where Copy T> pair => pair[USize :: 0]\n",
            "let direct: I32 = first (1, 2)\n",
        ),
        concat!(
            "def apply: <effect E> (() ->{E} I32) ->{E} I32 = <effect E> f => f ()\n",
            "let direct = apply (() => 42)\n",
        ),
        concat!(
            "use std.coroutine.*\n",
            "def launch: <T> move T -> Coroutine{}<I32> = <T> move value => coro { 1 }\n",
            "let direct = launch 3\n",
        ),
        concat!(
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T ^ N) = <T, N where Copy T, Natural N> value => n => (value ^ N)\n",
            "let direct: (I32 ^ 3) = repeat 7 3\n",
        ),
    ] {
        lower(&type_check(source));
    }
}

#[test]
fn schemes_are_visible_on_bindings_and_lambdas() {
    let module = type_check(
        "def id: <T> move T -> T = <T> move x => x\nlet alias = id\nlet annotated: <U> move U -> U = <U> move x => x\n",
    );
    for item in &module.syntax().items {
        let Item::Binding(binding) = item else {
            continue;
        };
        if binding.name == "alias" || binding.name == "annotated" {
            assert_eq!(binding.is_generic(), binding.name == "annotated");
            let symbol = module.symbol_for(binding.syntax.id).unwrap();
            assert!(matches!(
                module.type_of_symbol(symbol),
                Some(CheckedType::Forall(_))
            ));
            assert!(module.scheme_of_symbol(symbol).is_some());
        }
        if binding.name == "annotated" {
            let function = module
                .function_for(binding.value.as_ref().unwrap().syntax().id)
                .unwrap();
            assert!(module.scheme_of_function(function).is_some());
        }
    }
}

#[test]
fn lowering_ignores_unused_generic_bodies_with_polymorphic_locals() {
    let module = type_check(concat!(
        "def id: <U> move U -> U = <U> move x => x\n",
        "def unused: <T> move T -> T = <T> move x => { let f = id; x }\n",
        "let answer = id 42\n",
    ));
    lower(&module);
}

#[test]
fn lowering_keeps_explicit_and_expected_instantiations_concrete() {
    let module = type_check(concat!(
        "def id: <T> move T -> T = <T> move x => x\n",
        "let explicit = id.<I32>\n",
        "let inferred: move I32 -> I32 = id\n",
        "let answer = explicit 42\n",
    ));
    lower(&module);
}

#[test]
fn lowering_keeps_generic_capture_dependencies_demand_driven() {
    let module = type_check(concat!(
        "def id: <U> move U -> U = <U> move x => x\n",
        "def unused: <T where Copy T> T -> T = <T where Copy T> x => {\n",
        "  let inner = () => { let poly = id; let saved = x; 42 }\n",
        "  x\n",
        "}\n",
        "let answer = id 42\n",
    ));
    lower(&module);
}

#[test]
fn generic_bodies_check_bounds_on_their_own_parameters() {
    const COPIED: &str = "def copied: <T where Copy T> T -> T = <T where Copy T> x => x\n";
    let source = format!("{COPIED}def outer: <A> A -> A = <A> a => copied a\n");
    let diagnostics = TypeChecker::new()
        .check(resolve(&source))
        .expect_err_diagnostics("an unbounded body cannot satisfy `Copy A`");
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>(),
        vec!["trait bound is not satisfied for `A`"]
    );
    type_check(&format!(
        "{COPIED}def outer: <A where Copy A> A -> A = <A where Copy A> a => copied a\nlet answer = outer 42\n"
    ));
}

#[test]
fn effect_parameters_bind_to_rigid_effect_rows() {
    const APPLY: &str = "def apply: <effect E> (() ->{E} I32) ->{E} I32 = <effect E> f => f ()\n";
    type_check(&format!(
        "{APPLY}let alias: <effect F> (() ->{{F}} I32) ->{{F}} I32 = apply\n"
    ));
    let source = format!("{APPLY}let bad: <effect F> (() ->{{F}} I32) -> I32 = apply\n");
    let diagnostics = TypeChecker::new()
        .check(resolve(&source))
        .expect_err_diagnostics("the result row must keep `F`");
    assert!(
        diagnostics.iter().any(|d| d
            .message
            .starts_with("generic function conflicts with the expected type")),
        "{diagnostics:?}"
    );
}

#[test]
fn lowering_admits_polymorphic_values_stored_and_called() {
    let module = type_check(concat!(
        "def identity: <T> move T -> T = <T> move value => value\n",
        "def consume: (<T> move T -> T) -> I32 = f => f 1\n",
        "type Functions wrap (identity: <T> move T -> T,)\n",
        "let r = (consume identity) + 1\n",
        "let fs = Functions (identity: identity)\n",
        "let s = fs.identity 2\n",
    ));
    lower(&module);
}

#[test]
fn resolves_independently_generic_trait_member_scopes() {
    resolve(
        "trait Visit T { visit: <A> (T, A) -> A = <A> (_, value) => { let item: A = value; item } }\n",
    );
}

#[test]
fn checks_independently_generic_trait_member_declarations() {
    TypeChecker::new()
        .check(resolve("trait Visit T { visit: <A> (T, A) -> A }\n"))
        .expect("member scheme should check");
    TypeChecker::new()
        .check(resolve(
            "trait Apply T { apply: T -> (<A> move A -> A) -> T }\n",
        ))
        .expect("nested member scheme should check");
}

#[test]
fn checks_generic_trait_member_conformance() {
    TypeChecker::new().check(resolve("trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = <A> (_, move value) => value }\n"))
        .expect("implementation must work for every member argument");
    let diagnostics = TypeChecker::new().check(resolve("trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = <A> (_, move value) => 42 }\n"))
        .expect_err_diagnostics("implementation cannot fix A to I32");
    assert!(
        diagnostics.iter().any(|d| d.message.contains("Visit.visit")
            && d.message.contains("scheme `<A>")
            && d.message.contains("expected")),
        "{diagnostics:?}"
    );
}

#[test]
fn infers_generic_trait_member_arguments_and_explicit_order() {
    TypeChecker::new()
        .check(resolve(include_str!(
            "fixtures/generic_trait_members/static.sta"
        )))
        .expect("static generic member calls should check");
    TypeChecker::new().check(resolve("trait Source T { type Element; read: T -> Element }\nimpl Source I32 { type Element = I32; read = x => x }\nlet x = Source.read.<I32> 1\n"))
        .expect("associated parameters are omitted from explicit argument order");
}

#[test]
fn rejects_polymorphic_recursion_through_generic_trait_dispatch() {
    let diagnostics = TypeChecker::new().check(resolve("trait Grow T { grow: <A where Copy A> (T, A) -> () = <A where Copy A> (item, value) => Grow.grow (item, (value, value)) }\n"))
        .expect_err_diagnostics("dispatch cannot recursively expand member arguments");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("polymorphic recursion")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_polymorphic_recursion_through_indirect_calls_in_lowering() {
    // Each program keeps wrapping the member argument through a second
    // function, which the checker's self-use rule cannot see. Lowering used to
    // request ever deeper instances without end.
    for source in [
        concat!(
            "type Wrap<A> wrap A\n",
            "trait Grow T {\n",
            "    grow: <A> (T, USize, move A) -> USize\n",
            "    shrink: <A> (T, USize, move A) -> USize = <A> (item, n, move value) => match n == 0 { True => 0, False => Grow.grow (item, n - 1, Wrap value) }\n",
            "}\n",
            "impl Grow I32 { grow = <A> (item, n, move value) => Grow.shrink (item, n, value) }\n",
            "let total = Grow.grow (1, 3, 1)\n",
        ),
        concat!(
            "type Wrap<A> wrap A\n",
            "trait Grow T { grow: <A> (T, USize, move A) -> USize }\n",
            "def helper: <T, A where Grow T> (T, USize, move A) -> USize = <T, A where Grow T> (item, n, move value) => match n == 0 { True => 0, False => Grow.grow (item, n - 1, Wrap value) }\n",
            "impl Grow I32 { grow = <A> (item, n, move value) => helper (item, n, value) }\n",
            "let total = Grow.grow (1, 3, 1)\n",
        ),
        concat!(
            "type Wrap<A> wrap A\n",
            "def first: <A> (USize, move A) -> USize = <A> (n, move value) => match n == 0 { True => 0, False => second (n - 1, Wrap value) }\n",
            "def second: <A> (USize, move A) -> USize = <A> (n, move value) => first (n, value)\n",
            "let total = first (3, 1)\n",
        ),
    ] {
        let module = type_check(source);
        let diagnostics = Lowerer::new()
            .lower(&module)
            .expect_err_diagnostics("indirect polymorphic recursion should fail lowering");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .starts_with("polymorphic recursion is not supported: instantiating `")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn lowers_dispatch_chains_that_end_at_a_deeper_type() {
    // A chain that reaches a deeper type and stops there is not recursion.
    let module = type_check(concat!(
        "type Box<T> wrap T\n",
        "trait Step T { step: T -> I32 }\n",
        "def run: <T where Step T> T -> I32 = <T where Step T> value => Step.step value\n",
        "impl Step I32 { step = value => run (Box value) }\n",
        "impl Step Box<I32> { step = _ => 1 }\n",
        "let total = run 1\n",
    ));
    lower(&module);
}

#[test]
fn checks_polymorphic_trait_members_and_keeps_projection_restriction() {
    TypeChecker::new()
        .check(resolve(include_str!(
            "fixtures/generic_trait_members/values.sta"
        )))
        .expect("member values instantiate independently and subsume bounds");
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "trait Source T { type Element; read: T -> Element }\nlet member = Source.read\n",
        ))
        .expect_err_diagnostics("associated projections remain unsupported in value schemes");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("associated type")),
        "{diagnostics:?}"
    );
    TypeChecker::new()
        .check(resolve("trait Source T { type Element; keep: T -> T }\n"))
        .expect("only explicit trait parameters must occur in each member");
}

#[test]
fn trait_member_parameters_cannot_reuse_trait_parameter_names() {
    for (source, name) in [
        ("trait Shadow T { shadow: <T> (T, move T) -> T }\n", "T"),
        (
            "trait Source T { type Element; read: <Element> (T, move Element) -> Element }\n",
            "Element",
        ),
    ] {
        let diagnostics = resolve_result(source)
            .expect_err_diagnostics("a member parameter cannot hide the trait's");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            diagnostics[0].message.ends_with(&format!(
                "cannot declare parameter `{name}`: the trait already declares `{name}`"
            )),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn trait_member_parameters_admit_slotted_values() {
    let declarations = concat!(
        "trait Run T { run: <A> (T, A -> I32) -> I32 }\n",
        "impl Run I32 { run = <A> (_, f) => 1 }\n",
        "trait Measure T { measure: <A> T -> USize }\n",
        "impl Measure I32 { measure = <A> _ => 4 }\n",
    );
    for use_site in [
        "let ran = Run.run (1, (value: I32) => value)\nlet size = Measure.measure.<_, String> 1\nlet member = Measure.measure\n",
        "def add: (I32; I32) -> I32 = (left; right) => left + right\nlet ran = Run.run (1, add)\nlet size = Measure.measure.<_, (I32; I32)> 1\n",
    ] {
        let module = type_check(&format!("{declarations}{use_site}"));
        let context = Context::create();
        CodeGenerator::new(&context)
            .compile_root(&lower(&module))
            .expect("trait-member generics admit slotted callback parameters and type arguments");
    }
}

#[test]
fn explicitly_instantiated_slotted_trait_members_take_juxtaposed_arguments() {
    let declarations = concat!(
        "trait Pick T { pick: (T; T) -> T }\n",
        "impl Pick I32 { pick = (left; _) => left }\n",
        "trait Pair T { pair: <A> (T; move A) -> A }\n",
        "impl Pair I32 { pair = <A> (_; move value) => value }\n",
    );
    for use_site in [
        "let picked = Pick.pick.<I32> 1 2\n",
        "let picked = Pick.pick.<I32> ...(1; 2)\n",
        "let paired = Pair.pair.<I32, String> 1 \"text\"\n",
    ] {
        let module = type_check(&format!("{declarations}{use_site}"));
        let context = Context::create();
        CodeGenerator::new(&context)
            .compile_root(&lower(&module))
            .expect("explicit type arguments keep a trait member's slots");
    }
}

#[test]
fn plans_generic_trait_member_entries_only_when_reachable() {
    let declarations = "trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = <A> (_, move value) => value }\n";
    for use_site in [
        "let answer = Visit.visit (1, 42)\n",
        "def unused: <T where Visit T> T -> () = <T where Visit T> item => { let member = Visit.visit; () }\nlet answer = 42\n",
    ] {
        let lowered = lower(&type_check(&format!("{declarations}{use_site}")));
        assert!(
            lowered
                .planned_artifact_names(PlannedArtifactFamily::TraitMemberEntry)
                .is_empty(),
            "static calls and unreachable templates must not request member entries"
        );
    }
    let lowered = lower(&type_check(&format!(
        "{declarations}let member = Visit.visit\nlet answer = member (1, 42)\n"
    )));
    assert!(
        !lowered
            .planned_artifact_names(PlannedArtifactFamily::TraitMemberEntry)
            .is_empty()
    );
}

#[test]
fn implementation_bodies_do_not_inherit_member_parameter_names() {
    let diagnostics = resolve_result("trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = <B> (_, move value) => { let saved: A = value; saved } }\n")
        .expect_err_diagnostics("member parameter names are private to the declaration");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("A") && d.message.contains("unknown")),
        "{diagnostics:?}"
    );
}

#[test]
fn generic_trait_members_infer_skipped_own_arguments_and_keep_phantom_arguments() {
    let declarations = "trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = <A> (_, move value) => value }\ntrait Read T { read: <A where Copy A> T -> T }\nimpl Read I32 { read = <A where Copy A> value => value }\n";
    let source = format!(
        "{declarations}let answer = Visit.visit.<I32, _> (1, 42)\nlet phantom = Read.read.<I32, String> 1\n"
    );
    lower(&type_check(&source));
    let stored = lower(&type_check(&format!(
        "{declarations}let member = Read.read\nlet valid = member.<I32, String> 1\nlet instantiated: I32 -> I32 = member.<I32, String>\nlet result = instantiated 2\n"
    )));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&stored)
        .expect("explicit phantom arguments reach instantiation adapters");
    for use_site in [
        "let result = Read.read 1\n",
        "let result = Read.read.<I32, _> 1\n",
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(&format!("{declarations}{use_site}")))
            .expect_err_diagnostics("phantom member parameters need explicit arguments");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("could not infer all")),
            "{diagnostics:?}"
        );
    }
    for use_site in [
        "let member = Read.read\nlet invalid = member 1\n",
        "let member = Read.read\nlet invalid = member.<I32, _> 1\n",
        "let member = Read.read\nlet invalid: I32 -> I32 = member\n",
        "let member: <U, B where Read U, Copy B> U -> U = Read.read\nlet invalid = member 1\n",
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(&format!("{declarations}{use_site}")))
            .expect_err_diagnostics("stored phantom parameters also require explicit arguments");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("could not infer all")),
            "{diagnostics:?}"
        );
    }
    let bounded = "trait Bound T {}\nimpl Bound I32 {}\ntrait Read T { read: <A where Bound A> T -> T }\nimpl Read I32 { read = <A where Bound A> value => value }\n";
    for use_site in [
        "let invalid = Read.read.<I32, Bool> 1\n",
        "let member = Read.read\nlet invalid = member.<I32, Bool> 1\n",
        "let member = Read.read\nlet invalid: I32 -> I32 = member.<I32, Bool>\n",
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(&format!("{bounded}{use_site}")))
            .expect_err_diagnostics("explicit phantom arguments still discharge member bounds");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("trait bound")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn generic_trait_member_bounds_check_completed_associated_arguments() {
    let declarations = "trait Bound T {}\nimpl Bound I32 {}\ntrait Source T { type Element; check: <A where Copy A, Bound Element> T -> T }\nimpl Source I32 { type Element = I32; check = <A where Copy A, Bound I32> value => value }\nimpl Source Bool { type Element = String; check = <A where Copy A, Bound String> value => value }\n";
    lower(&type_check(&format!(
        "{declarations}let valid = Source.check.<I32, I32> 1\n"
    )));
    let diagnostics = TypeChecker::new()
        .check(resolve(&format!(
            "{declarations}let invalid = Source.check.<Bool, I32> (Bool :: True)\n"
        )))
        .expect_err_diagnostics("member bounds must use completed associated types");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("trait bound")),
        "{diagnostics:?}"
    );
}

#[test]
fn generic_trait_members_preserve_product_trait_parameters() {
    let module = type_check(concat!(
        "trait Pair (Left, Right) { carry: <A> (Left, Right, move A) -> A = <A> (_, _, move value) => value }\n",
        "impl Pair (I32, String) {}\n",
        "let direct = Pair.carry.<(I32, String), I32> (1, \"right\", 42)\n",
        "let member = Pair.carry\n",
        "let stored = member.<(I32, String), Bool> (1, \"right\", (Bool :: True))\n",
        "def relay: <L, R, A where Pair (L, R)> (L, R, move A) -> A = <L, R, A where Pair (L, R)> (left, right, move value) => Pair.carry.<(L, R), A> (left, right, value)\n",
        "let generic = relay\n",
        "let result = generic (1, \"right\", 7)\n",
        "trait AssociatedPair (Left, Right) { type Output; carry: <A> (Left, Right, move A) -> (Output, A) }\n",
        "impl AssociatedPair (I32, String) { type Output = Bool; carry = <A> (_, _, move value) => ((Bool :: True), value) }\n",
        "let associated = AssociatedPair.carry.<(I32, String), I32> (1, \"right\", 8)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("product trait parameters remain grouped before member arguments");
}

#[test]
fn pipe_stages_resolve_companions_traits_and_qualified_members() {
    let module = type_check(concat!(
        "type Boxed wrap I32\n",
        "companion Boxed { pub def step: Boxed -> I32 -> Boxed = value => _ => value }\n",
        "trait Finish T { finish: T -> I32 -> I32 }\n",
        "impl Finish Boxed { finish = Boxed value => n => value + n }\n",
        "let value = Boxed 3\n",
        "let chained: I32 = value |> step 1 |> finish 2\n",
        "let qualified: I32 = value |> Boxed.step 1 |> Finish.finish 2\n",
        "let explicit_trait: I32 = value |> Finish.finish.<Boxed> 2\n",
        "let grouped: I32 = value |> (step 1) |> (finish 2)\n",
        "let partial: I32 -> I32 = value |> finish\n",
        "companion Boxed { pub def tag: <T> Boxed -> T -> I32 = <T> _ => _ => 4 }\n",
        "let generic: I32 = value |> tag.<I32> 2\n",
        "trait Associated T { type Output; convert: T -> Output }\n",
        "impl Associated Boxed { type Output = I32; convert = Boxed value => value }\n",
        "let associated: I32 = value |> convert\n",
        "trait Pair T { first: T -> I32 }\n",
        "impl Pair (I32, I32) { first = (a, _) => a }\n",
        "let structural: I32 = (1, 2) |> first\n",
        "trait Hop T { hop: (T; I32) -> T }\n",
        "impl Hop Boxed { hop = (value; _) => value }\n",
        "let juxtaposed: Boxed = value |> hop 2\n",
        "companion Boxed { pub def marked: <T> (Boxed; T) -> Boxed = <T> (value; _) => value }\n",
        "let explicit_slots: Boxed = value |> marked.<I32> 2\n",
        "let explicit_trait_slots: Boxed = value |> Hop.hop.<Boxed> 2\n",
        "mod Helpers { pub trait Render T { render: T -> String } }\n",
        "use Helpers.(Render)\n",
        "impl Render Boxed { render = _ => \"box\" }\n",
        "let imported: String = value |> render\n",
        "def bounded: <T where Finish T> T -> I32 = <T where Finish T> value => value |> finish 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("pipes should lower");
}

#[test]
fn pipe_lookup_prefers_compatible_companions_and_falls_back_to_traits() {
    type_check(concat!(
        "type Boxed wrap I32\n",
        "companion Boxed { pub def pick: Boxed -> String -> String = _ => text => text }\n",
        "trait Pick T { pick: T -> I32 -> I32 }\n",
        "impl Pick Boxed { pick = Boxed value => n => value + n }\n",
        "trait OtherPick T { pick: T -> String -> I32 }\n",
        "impl OtherPick Boxed { pick = _ => _ => 7 }\n",
        "let value = Boxed 3\n",
        "let preferred: String = value |> pick \"text\"\n",
        "let fallback: I32 = value |> pick 2\n",
    ));
}

#[test]
fn pipe_stages_reject_ambiguous_incomplete_and_non_method_calls() {
    let cases = [
        (
            concat!(
                "trait First T { pick: T -> I32 }\n",
                "trait Second T { pick: T -> I32 }\n",
                "impl First I32 { pick = value => value }\n",
                "impl Second I32 { pick = value => value }\n",
                "let result = 1 |> pick\n",
            ),
            "ambiguous trait method",
        ),
        (
            concat!(
                "trait Hop T { hop: (T; I32) -> T }\n",
                "impl Hop I32 { hop = (value; _) => value }\n",
                "let result = 1 |> hop\n",
            ),
            "juxtaposed",
        ),
        (
            "mod Helpers { pub def identity: I32 -> I32 = value => value }\nlet result = 1 |> Helpers.identity\n",
            "qualified pipe stage",
        ),
        (
            "def identity: String -> String = value => value\nlet result = 1 |> undefined\n",
            "no compatible",
        ),
        (
            "trait Missing T { missing: T -> I32 }\nlet result = 1 |> missing\n",
            "no trait implementation",
        ),
        (
            concat!(
                "trait Hop T { hop: (T; I32) -> T }\n",
                "impl Hop I32 { hop = (value; _) => value }\n",
                "let result = 1 |> Hop.hop.<String> 2\n",
            ),
            "no trait implementation",
        ),
        (
            concat!(
                "trait Pair T { pair: (T, T) -> T }\n",
                "impl Pair I32 { pair = (left, _) => left }\n",
                "let result = 1 |> pair 2\n",
            ),
            "trait method",
        ),
        (
            concat!(
                "mod Hidden { pub trait Show T { show: T -> I32 }; impl Show I32 { show = value => value } }\n",
                "let result = 1 |> show\n",
            ),
            "no compatible",
        ),
    ];
    for (source, message) in cases {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("invalid stage must fail");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(message)),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn slotted_products_typecheck_as_distinct_values_and_function_parameters() {
    let module = type_check(concat!(
        "def add: (I32; I32) -> I32 = (x; y) => x + y\n",
        "let sum: I32 = add 1 2\n",
        "let slots: (I32; I32) = (1; 2)\n",
        "let plain: (I32, I32) = (...slots)\n",
        "let again: (I32; I32) = (...plain;)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slotted products should lower as products");
    let errors = type_check_errors(
        "let wrong: (I32; I32) = (1, 2)\n",
        "product kinds are distinct",
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("expected `(I32; I32)`")),
        "{errors:?}"
    );
}

#[test]
fn slotted_type_spreads_preserve_target_kind_and_validate_substitutions() {
    let module = type_check(concat!(
        "type Append<P> = (...P; I32)\n",
        "type Plain<P> = (...P, I32)\n",
        "type Slots<P> = (...P;)\n",
        "let slotted: Append<(I32, I32)> = (1; 2; 3)\n",
        "let ordinary: Plain<(I32; I32)> = (1, 2, 3)\n",
        "let converted: Slots<(I32, I32)> = (1; 2)\n",
        "let single: Append<()> = 3\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("type spreads should specialize to fixed products");
    for source in [
        "type Append<P> = (...P; I32)\nlet bad: Append<I32> = (1; 2)\n",
        "type Slots<P> = (...P;)\nlet bad: Slots<()> = ()\n",
        "type Empty = (...(I32 ^ 0);)\nlet bad: Empty = ()\n",
    ] {
        let errors = type_check_errors(source, "type spread must have a valid fixed shape");
        assert!(
            errors
                .iter()
                .any(|message| message.contains("cannot spread non-product")
                    || message.contains("at least one slot")),
            "{errors:?}"
        );
    }
}

#[test]
fn slotted_function_markers_follow_expanded_type_slots() {
    let module = type_check(concat!(
        "def replace: (...(I32, I32); mut I32) -> () = (a; b; mut value) => { value = a + b }\n",
        "let mut result: I32 = 0\n",
        "replace 1 2 result\n",
        "def singleton: (mut I32;) -> () = mut value => { value = 4 }\n",
        "singleton result\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("expanded slot markers compile");
}

#[test]
fn slotted_calls_hint_to_spread_a_whole_parameter_value() {
    for source in [
        "def add: (I32; I32) -> I32 = (a; b) => a + b\nlet p: (I32; I32) = (1; 2)\nadd p\n",
        "def add: (I64; I64) -> I64 = (a; b) => a + b\nadd (1; 2)\n",
        "def add: (I32; I32) -> I32 = (a; b) => a + b\nlet wrapped = (pair: (1; 2), other: 3)\nadd wrapped.pair\n",
        "def add: (I32; I32) -> I32 = (a; b) => a + b\nlet f: (I32; I32) -> I32 = add\nlet x: (I32; I32) = (1; 2)\nf x\n",
        "def first: <T> (T; T) -> T = <T> (a; _) => a\nlet p: (I32; I32) = (1; 2)\nfirst.<I32> p\n",
        "trait Choose T { choose: (T; T) -> T }\nimpl Choose I32 { choose = (a; _) => a }\nlet p: (I32; I32) = (1; 2)\nChoose.choose.<I32> p\n",
        "type Animal from I32\ncompanion Animal { pub def choose: (Animal; Animal) -> Animal = (a; _) => a }\nlet a: Animal = 1\nlet p: (Animal; Animal) = (a; a)\nAnimal.choose p\n",
        "trait Choose T { choose: (T; T) -> T }\nimpl Choose I32 { choose = (a; _) => a }\nlet p: (I32; I32) = (1; 2)\np |> Choose.choose.<I32>\n",
        "def add: (x: I32; y: I32) -> I32 = (x; y) => x + y\nlet p = (1; 2)\nadd p\n",
        "def add: (x: I32; y: I32) -> I32 = (x; y) => x + y\ndef g = (p: (I32; I32)) => add p\n",
        "def add: (x: I32; y: I32) -> I32 = (x; y) => x + y\ndef main = () => {\n    let p = (1; 2)\n    add p\n}\n",
    ] {
        let messages = type_check_errors(source, "the product fills one argument, not two slots");
        assert!(
            messages
                .iter()
                .any(|message| message.contains("spread it into the slots:")),
            "{source}: {messages:?}"
        );
    }
    type_check(concat!(
        "def first: ((I32; I32); I32) -> I32 = (pair; _) => pair.0\n",
        "let pair: (I32; I32) = (1; 2)\n",
        "let value: I32 = first pair 3\n",
        "let literal: I32 = first (1; 2) 3\n",
    ));
}

#[test]
fn slotted_diagnostics_quote_the_call_and_slot_separators() {
    let messages = type_check_errors(
        "def add: (x: I32; y: I32) -> I32 = (x; y) => x + y\nlet p = (1; 2)\nadd p\n",
        "a whole slotted value fills one slot",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("`add ...p`")),
        "{messages:?}"
    );
    let messages = type_check_errors(
        "def add: (I32; I32) -> I32 = (a; b) => a + b\nadd (1; 2)\n",
        "a slotted literal fills one slot",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("`add ...(1; 2)`")),
        "{messages:?}"
    );
    let messages = type_check_errors(
        "def add: (I32; I32) -> I32 = (a; b) => a + b\nadd 1 ...(2, 3)\n",
        "a spread overruns the last slot",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("spread fills 2 slots but only 1 slot remains")),
        "{messages:?}"
    );
    let messages = type_check_errors(
        "def first: <A> (move A; I32) -> A = <A> (a; n) => a\n",
        "slot markers disagree with the annotation",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("(A; I32)") && message.contains("(move A; I32)")),
        "{messages:?}"
    );
}

#[test]
fn generic_constructors_take_slotted_values_as_one_slot() {
    let module = type_check(concat!(
        "type Box<T> wrap (value: T)\n",
        "let slots: (I32; I32) = (1; 2)\n",
        "let inferred = Box slots\n",
        "let expected: Box<(I32; I32)> = Box slots\n",
        "let literal: Box<(I32; I32)> = Box (1; 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("boxed slotted values compile");
    let messages = type_check_errors(
        "type Box<T> wrap (value: T)\nlet slots: (I32; I32) = (1; 2)\nlet b: Box<(I32; I32)> = Box ...slots\n",
        "a generic parameter is one slot",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("spread fills 2 slots but only 1 slot remains")),
        "{messages:?}"
    );
}

#[test]
fn slotted_call_spreads_reject_statically_unknown_or_non_product_values() {
    let unknown = type_check_errors(
        "def bad: <A> (A; (I32; I32) -> I32) -> I32 = <A> (value; f) => f ...value\n",
        "an opaque parameter has no known spread shape",
    );
    assert!(
        unknown
            .iter()
            .any(|message| message == "cannot spread a value of unknown shape"),
        "{unknown:?}"
    );
    let scalar = type_check_errors(
        "def add: (I32; I32) -> I32 = (a; b) => a + b\nlet value: I32 = 1\nadd ...value\n",
        "a scalar is not a spread operand",
    );
    assert!(
        scalar
            .iter()
            .any(|message| message.contains("cannot spread non-product value")),
        "{scalar:?}"
    );
}

#[test]
fn slotted_direct_calls_spread_fixed_products_into_consecutive_slots() {
    let module = type_check(concat!(
        "def add: (I64; I64) -> I64 = (a; b) => a + b\n",
        "let slotted: (I64; I64) = (1; 2)\n",
        "let plain: (I64, I64) = (3, 4)\n",
        "let first: I64 = add ...slotted\n",
        "let second: I64 = add ...plain\n",
        "let third: I64 = add ...(5; 6)\n",
        "def sum: (I32; I32; I32) -> I32 = (a; b; c) => a + b + c\n",
        "let fourth: I32 = sum 1 ...(2, 3)\n",
        "let fifth: I32 = sum ...(1, 2) 3\n",
        "let sixth: I32 = sum ...() 1 2 3\n",
        "let f: (I64; I64) -> I64 = add\n",
        "let seventh: I64 = f ...plain\n",
        "def same: <A where Copy A> (A; A) -> A = <A where Copy A> (a; _) => a\n",
        "let eighth: I32 = same ...(7, 8)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("call spreads emit LLVM");
}

#[test]
fn slotted_direct_call_spreads_cannot_overrun_or_fill_plain_product_slots() {
    for source in [
        "def f: (I32; I32) -> I32 = (a; b) => a + b\nf 1 ...(2, 3)\n",
        "def f: (I32, I32) -> I32 = (a, b) => a + b\nf ...(1, 2)\n",
    ] {
        let messages = type_check_errors(source, "spreads cannot overrun a static slot count");
        assert!(
            messages.iter().any(|message| message.contains("but only")),
            "{messages:?}"
        );
    }
}

#[test]
fn generic_constructor_values_keep_their_expected_callable_types() {
    for name in [
        "dynamic_constructor_values",
        "census_constructor_adapters",
        "example_traits_and_generics",
    ] {
        let program = staple_compiler::codegen_corpus()
            .iter()
            .find(|program| program.name == name)
            .expect("corpus case");
        let source = match program.source {
            staple_compiler::CorpusSource::Inline(source) => source.to_owned(),
            staple_compiler::CorpusSource::File(path) => std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join(path),
            )
            .expect("corpus file"),
        };
        let module = type_check(&source);
        let context = Context::create();
        CodeGenerator::new(&context)
            .compile_root(&lower(&module))
            .unwrap_or_else(|diagnostics| panic!("{name}: {diagnostics:?}"));
    }
}

#[test]
fn slotted_trait_calls_spread_operands_and_keep_pipeline_first_slots() {
    let module = type_check(concat!(
        "trait Total T { total: (T; T; T) -> T }\n",
        "impl Total I32 { total = (a; b; c) => a + b + c }\n",
        "let plain: (I32, I32, I32) = (1, 2, 3)\n",
        "let slotted: (I32; I32; I32) = (4; 5; 6)\n",
        "let first: I32 = Total.total ...plain\n",
        "let second: I32 = Total.total ...slotted\n",
        "let third: I32 = Total.total 1 ...(2, 3)\n",
        "let fourth: I32 = 1 |> Total.total ...(2, 3)\n",
        "let fifth: I32 = Total.total.<I32> ...(1, 2, 3)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("trait spreads emit LLVM");
}

#[test]
fn slotted_trait_spreads_report_slot_overruns_and_incomplete_calls() {
    for (suffix, expected) in [
        ("Total.total 1 ...(2, 3)\n", "but only"),
        ("Total.total ...()\n", "expected 2 arguments, found 0"),
        ("Total.total ...1\n", "cannot spread non-product value"),
    ] {
        let source = format!(
            "trait Total T {{ total: (T; T) -> T }}\nimpl Total I32 {{ total = (a; b) => a + b }}\n{suffix}"
        );
        let messages = type_check_errors(&source, "trait spreads obey static slot counts");
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "{messages:?}"
        );
    }
}

#[test]
fn slotted_call_spreads_apply_extra_arguments_to_returned_functions() {
    let module = type_check(concat!(
        "def add: (I32; I32) -> I32 = (a; b) => a + b\n",
        "def choose: (I32; I32) -> (I32; I32) -> I32 = (a; b) => add\n",
        "let first: I32 = choose ...(1, 2) ...(3, 4)\n",
        "let second: I32 = choose 1 2 3 4\n",
        "trait Choose T { choose: (T; T) -> (T; T) -> T }\n",
        "impl Choose I32 { choose = (a; b) => add }\n",
        "let third: I32 = Choose.choose ...(1, 2) ...(3, 4)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("returned slotted functions use fresh call groups");
}

#[test]
fn slotted_companion_calls_spread_remaining_slots() {
    let module = type_check(concat!(
        "type Animal from I32\n",
        "companion Animal {\n",
        "    pub def teleport: (Animal; I64; I64) -> Animal = (animal; _; _) => animal\n",
        "    pub def tag: <T> (Animal; T; T) -> Animal = <T> (animal; _; _) => animal\n",
        "}\n",
        "let animal: Animal = 1\n",
        "let plain: (I64, I64) = (2, 3)\n",
        "let slotted: (I64; I64) = (4; 5)\n",
        "let first: Animal = animal |> teleport ...plain\n",
        "let second: Animal = animal |> teleport ...slotted\n",
        "let third: Animal = animal |> teleport ...(6, 7)\n",
        "let fourth: Animal = animal |> Animal.teleport ...(8, 9)\n",
        "let fifth: Animal = Animal.teleport animal ...plain\n",
        "let sixth: Animal = animal |> tag ...(1, 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("companion spreads preserve receiver-first calls and literal context");
}

#[test]
fn slotted_companion_spreads_reject_overruns_and_unknown_shapes() {
    for (suffix, expected) in [
        ("animal |> teleport ...(1, 2, 3)\n", "but only"),
        (
            "animal |> teleport ...1\n",
            "cannot spread non-product value",
        ),
        (
            "def invalid: <T> T -> Animal = <T> value => animal |> teleport ...value\n",
            "unknown shape",
        ),
    ] {
        let source = format!(
            "type Animal from I32\ncompanion Animal {{ pub def teleport: (Animal; I32; I32) -> Animal = (animal; _; _) => animal }}\nlet animal: Animal = 1\n{suffix}"
        );
        let messages = type_check_errors(&source, "companion spreads obey static slot counts");
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "{messages:?}"
        );
    }
}

#[test]
fn overloaded_slotted_trait_calls_contextualize_candidate_slots() {
    let module = type_check(concat!(
        "trait Numeric T { compute: (T; I64; () -> I64) -> I64 }\n",
        "trait Logical T { compute: (T; Bool; () -> Bool) -> Bool }\n",
        "impl Numeric I32 { compute = (_; value; callback) => value + callback () }\n",
        "impl Logical Bool { compute = (_; value; _) => value }\n",
        "let receiver: I32 = 1\n",
        "let value: I64 = compute receiver 2 { 3 }\n",
        "let spread: I64 = compute ...(receiver, 2) { 3 }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("each overload supplies its own ordinary slot context");
    let messages = type_check_errors(
        concat!(
            "trait First T { choose: (T; I64) -> I64 }\n",
            "trait Second T { choose: (T; I64) -> I64 }\n",
            "impl First I32 { choose = (_; value) => value }\n",
            "impl Second I32 { choose = (_; value) => value }\n",
            "let receiver: I32 = 1\n",
            "let value: I64 = choose receiver 2\n",
        ),
        "two viable contextual candidates remain ambiguous",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("ambiguous trait method")),
        "{messages:?}"
    );
}

#[test]
fn slotted_trait_calls_contextualize_each_ordinary_slot() {
    let module = type_check(concat!(
        "trait Compute T { compute: (T; I64; () -> I64) -> I64 }\n",
        "impl Compute I32 { compute = (_; value; callback) => value + callback () }\n",
        "let receiver: I32 = 1\n",
        "let first: I64 = Compute.compute receiver 2 { 3 }\n",
        "let second: I64 = Compute.compute ...(receiver, 2) { 3 }\n",
        "let third: I64 = receiver |> Compute.compute 2 { 3 }\n",
        "def direct: (I64; () -> I64) -> I64 = (value; callback) => value + callback ()\n",
        "let fourth: I64 = direct 2 { 3 }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("trait slots apply literal context and block arguments independently");
}

#[test]
fn slotted_values_support_patterns_access_and_designated_initializers() {
    let module = type_check(concat!(
        "type Slots = (first: I32; middle: (I32, I32); last: I32)\n",
        "let slots: Slots = (.last: 4; .middle: (2, 3); .first: 1)\n",
        "let (a; b, c; d) = slots\n",
        "let whole: Slots = slots\n",
        "let result: I32 = whole.first + whole.middle.0 + (whole.1).1 + whole.last\n",
        "def sum: Slots -> I32 = value => value.first + value.middle.0 + value.middle.1 + value.last\n",
        "def destructure: Slots -> I32 = (a; b, c; d) => a + b + c + d\n",
        "def matched: Slots -> I32 = value => match value { (a; b, c; d) => a + b + c + d }\n",
        "let first: I32 = sum ...slots\n",
        "let second: I32 = destructure ...slots\n",
        "let third: I32 = matched ...slots\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slotted value patterns and access emit LLVM");
}

#[test]
fn slotted_constant_products_fold_and_convert_in_both_directions() {
    let module = type_check(concat!(
        "const slots: (x: I32; y: I32) = (x: 1 + 1; y: 2 + 2)\n",
        "const plain: (x: I32, y: I32) = (...slots)\n",
        "const restored: (x: I32; y: I32) = (...plain;)\n",
        "def read: () -> I32 = () => slots.x + slots.y + plain.0 + restored.1\n",
    ));
    for (name, slotted) in [("slots", true), ("plain", false), ("restored", true)] {
        let value = module
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.as_ref(),
                _ => None,
            })
            .expect("folded constant binding");
        let staple_syntax::Expression::Product(product) = value else {
            panic!("constant must fold to a product literal");
        };
        assert_eq!(product.slotted, slotted);
        assert_eq!(product.elements.len(), 2);
        for (element, literal) in product.elements.iter().zip(["2", "4"]) {
            assert!(
                matches!(&element.value, staple_syntax::Expression::Integer(integer) if integer.literal == literal)
            );
        }
    }
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slotted constant products fold through conversion spreads");
    assert!(llvm.contains("i32 2"));
    assert!(llvm.contains("i32 4"));
}

#[test]
fn slotted_values_support_structural_copy_and_debug() {
    let module = type_check(concat!(
        "use std.fmt.Formatter\n",
        "def copied: <T where Copy T> T -> T = <T where Copy T> value => value\n",
        "let slots: (I32; I32) = (1; 2)\n",
        "let same: (I32; I32) = copied slots\n",
        "let shown: String = Formatter.debug same\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slotted values synthesize structural trait implementations");
}

#[test]
fn product_display_is_structural_for_plain_and_slotted_values() {
    let module = type_check(concat!(
        "use std.fmt.Formatter\n",
        "def show: <T where Display T> T -> String = <T where Display T> value => Formatter.display value\n",
        "let plain: (I32, I32) = (1, 2)\n",
        "let slots: (I32; I32) = (3; 4)\n",
        "let nested: (I32; (I32, I32)) = (5; plain)\n",
        "let first: String = show plain\n",
        "let second: String = show slots\n",
        "let third: String = show nested\n",
        "let fourth: String = slots |> to_string\n",
        "let fifth: String = Formatter.display ()\n",
        "def show_slots: <T where Display T> (T; T) -> String = <T where Display T> value => Formatter.display value\n",
        "let sixth: String = show_slots 6 7\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("product Display derives delegates through generic dictionaries");
}

#[test]
fn product_display_requires_element_traits_and_rejects_overrides() {
    let messages = type_check_errors(
        concat!(
            "use std.fmt.Formatter\n",
            "let callback: () -> I32 = () => 1\n",
            "let slots: (I32; () -> I32) = (1; callback)\n",
            "let shown: String = Formatter.display slots\n",
        ),
        "product Display requires Display for every slot",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("trait bound is not satisfied")),
        "{messages:?}"
    );
    let messages = type_check_errors(
        "impl Display (I32; I32) { fmt = (value, mut formatter) => () }\n",
        "structural product Display cannot be overridden",
    );
    assert!(
        messages.iter().any(
            |message| message.contains("cannot implement an external trait for external types")
        ),
        "{messages:?}"
    );
}

#[test]
fn structural_product_equality_supports_plain_slotted_and_generic_values() {
    let module = type_check(concat!(
        "use std.cmp.Eq\n",
        "def compare: <T where Eq T> (T; T) -> Bool = <T where Eq T> (left; right) => left == right\n",
        "def compare_slots: <T where Eq T> ((T; T); (T; T)) -> Bool = <T where Eq T> (left; right) => left == right\n",
        "let plain: (I32, I32) = (1, 2)\n",
        "let slots: (I32; I32) = (3; 4)\n",
        "let named: (left: I32; right: String) = (left: 5; right: \"value\")\n",
        "let nested: (I32; (I32, I32)) = (6; plain)\n",
        "let first: Bool = plain == plain\n",
        "let second: Bool = slots != slots\n",
        "let third: Bool = named == named\n",
        "let fourth: Bool = nested == nested\n",
        "let fifth: Bool = compare slots slots\n",
        "let sixth: Bool = compare_slots slots slots\n",
        "let seventh: Bool = () == ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("product Eq derives element delegates through generic dictionaries");
}

#[test]
fn structural_product_equality_requires_element_traits_and_matching_kinds() {
    for source in [
        "let callback: () -> I32 = () => 1\nlet slots: (I32; () -> I32) = (1; callback)\nlet equal: Bool = slots == slots\n",
        "let slots: (I32; I32) = (1; 2)\nlet plain: (I32, I32) = (1, 2)\nlet equal: Bool = slots == plain\n",
    ] {
        let messages =
            type_check_errors(source, "product equality requires compatible Eq elements");
        assert!(!messages.is_empty());
    }
}

#[test]
fn slotted_values_derive_indexing_mutation_and_iteration() {
    let module = type_check(concat!(
        "let mut slots: (I32; I32) = (1; 2)\n",
        "let position: USize = 1\n",
        "let first: I32 = slots[position]\n",
        "slots[position] = 3\n",
        "let iterator: ((I32; I32), USize) = IntoIterator.into_iterator slots\n",
        "let step: IterStep<((I32; I32), USize), I32> = Iterator.next iterator\n",
        "let mixed: (I32; Bool) = (1; True)\n",
        "let indexed: I32 | Bool = mixed[position]\n",
        "let mixed_iterator: ((I32; Bool), USize) = IntoIterator.into_iterator mixed\n",
        "let mixed_step: IterStep<((I32; Bool), USize), I32 | Bool> = Iterator.next mixed_iterator\n",
        "let mut total: I32 = 0\n",
        "for value in slots { total = total + value }\n",
        "let plain: (I32, I32) = (...slots)\n",
        "let expanded: (I32; I32; I32) = (...plain; 4)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("slotted products derive the full product indexing and iteration paths");
}

#[test]
fn mutable_slotted_spreads_reject_immutable_product_roots() {
    for source in [
        "def change: (mut I32; I32) -> () = (mut value; _) => { value = 3 }\nlet pair: (I32; I32) = (1; 2)\nchange ...pair\n",
        "def change: (I32; mut I32; I32) -> () = (_; mut value; _) => { value = 3 }\nlet pair: (I32, I32) = (1, 2)\nchange 0 ...pair\n",
    ] {
        let messages =
            type_check_errors(source, "mutable spread projections require mutable roots");
        assert!(
            messages
                .iter()
                .any(|message| message.contains("not declared `mut`")),
            "{messages:?}"
        );
    }
}

#[test]
fn slotted_generic_opaque_calls_and_specialized_function_values_emit() {
    let module = type_check(concat!(
        "type Slots = (I32; I32)\n",
        "def add: Slots -> I32 = (a; b) => a + b\n",
        "def apply: <A, R> (A -> R; A) -> R = <A, R> (f; value) => f value\n",
        "def identity: <A where Copy A> A -> A = <A where Copy A> value => value\n",
        "let first: I32 = apply add (20; 22)\n",
        "let specialized: Slots -> Slots = identity.<Slots>\n",
        "let second: Slots = specialized 3 4\n",
        "let third: Slots = identity.<Slots> 5 6\n",
        "let stored = apply\n",
        "let fourth: I32 = stored add (7; 8)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("opaque generic calls adapt aggregate arguments to the shared slotted ABI");
}

#[test]
fn slotted_spreads_preserve_move_positions() {
    type_check(concat!(
        "use std.cinterop.*\n",
        "def consume: (I32; I32; move CString) -> () = (a; b; move text) => ()\n",
        "def valid = () => { let text = c_string \"x\"; consume ...(1; 2) text }\n",
        "def mixed: (move CString; CString) -> () = (move a; b) => ()\n",
        "def literal = () => { let first = c_string \"x\"; let second = c_string \"y\"; mixed ...(first; second); CString.to_string_unchecked second }\n",
    ));
    for body in [
        "consume ...(1; 2) text; CString.to_string_unchecked text",
        "mixed ...(text; c_string \"y\"); CString.to_string_unchecked text",
    ] {
        let source = format!(
            "use std.cinterop.*\ndef consume: (I32; I32; move CString) -> () = (a; b; move text) => ()\ndef mixed: (move CString; CString) -> () = (move a; b) => ()\ndef invalid = () => {{ let text = c_string \"x\"; {body} }}\n"
        );
        let diagnostics = TypeChecker::new()
            .check(resolve(&source))
            .expect_err_diagnostics("a spread must preserve consuming slot positions");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("use of moved value")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn slotted_macro_parameters_match_atomic_syntax() {
    let module = type_check(concat!(
        "macro apply: (Expr; Expr) -> Expr = (callee; argument) => parse_quote { $callee $argument }\n",
        "macro inferred = (callee: Expr; argument: Expr) => parse_quote { $callee $argument }\n",
        "macro grouped: (Parenthesized<Expr>; Expr) -> Expr = (left; right) => parse_quote { $left + $right }\n",
        "def identity: I32 -> I32 = value => value\n",
        "let first: I32 = apply identity 42\n",
        "let second: I32 = inferred identity 43\n",
        "let third: I32 = grouped (20) 22\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("annotated and inferred slotted macros emit expanded code");
}

#[test]
fn polymorphic_callback_values_admit_slotted_instantiations() {
    let module = type_check(concat!(
        "type Slots = (I32; I32)\n",
        "def sum: Slots -> I32 = (first; second) => first + second\n",
        "def run: <A> (A -> I32) -> I32 = <A> f => 42\n",
        "def consume: (<A> A -> I32) -> I32 = f => 42\n",
        "def constant: <A> A -> I32 = <A> _ => 0\n",
        "let stored = run\n",
        "let first: I32 = stored sum\n",
        "let second: I32 = consume constant\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect(
            "polymorphic callback values no longer carry parameter-product capability restrictions",
        );
}

#[test]
fn generic_whole_mutation_preserves_slotted_writeback() {
    static RESULT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    extern "C" fn record(value: i32) {
        RESULT.store(value, std::sync::atomic::Ordering::SeqCst);
    }
    let mut failures = Vec::new();
    for call in [
        "update.<Slots> ...pair\n",
        "let stored = update\nstored.<Slots> ...pair\n",
        "def borrow: <A where Transform A> mut A -> (() -> ()) = <A where Transform A> mut current => () => { current = Transform.transform current; () }\ndef scoped: () ->{state} I32 = () => { let mut local: Slots = (1; 2); { let apply = borrow.<Slots> ...local; apply () }; local.0 + local.1 }\npair = (scoped (); 0)\n",
        "def captured: <A where Transform A> mut A -> () = <A where Transform A> mut current => { let apply = () => { current = Transform.transform current; () }; apply () }\ncaptured.<Slots> ...pair\n",
        "def captured: <A where Transform A> mut A -> () = <A where Transform A> mut current => { let apply = () => { current = Transform.transform current; () }; apply () }\nlet stored = captured\nstored.<Slots> ...pair\n",
        "def forward: <A where Transform A> mut A -> () = <A where Transform A> mut current => update current\nforward.<Slots> ...pair\n",
        "def forward: <A where Transform A> mut A -> () = <A where Transform A> mut current => { if True { update current } else { () }; () }\nforward.<Slots> ...pair\n",
        "def forward: <A where Transform A> mut A -> () = <A where Transform A> mut current => { return update current; }\nlet mut left = (1; 100)\nlet mut right = (200; 2)\nforward.<Slots> left.0 right.1\npair = (left.0; right.1)\n",
        "let mut left = (1; 100)\nlet mut right = (200; 2)\nupdate.<Slots> left.0 right.1\npair = (left.0; right.1)\n",
        "def forward: <A where Transform A> mut A -> () = <A where Transform A> mut current => update current\nlet mut left = (1; 100)\nlet mut right = (200; 2)\nforward.<Slots> left.0 right.1\npair = (left.0; right.1)\n",
    ] {
        let module = type_check(&format!(
            "extern \"c\" {{ record: I32 -> () }}\n\
             type Slots = (I32; I32)\n\
             trait Transform T {{ transform: T -> T }}\n\
             impl Transform Slots {{ transform = pair => (pair.0 + 10; pair.1 + 20) }}\n\
             def update: <A where Transform A> mut A -> () = <A where Transform A> mut current => {{ current = Transform.transform current; () }}\n\
             let mut pair: Slots = (1; 2)\n\
             {call}\
             record (pair.0 + pair.1)\n"
        ));
        let context = Context::create();
        let text = match CodeGenerator::new(&context).compile_module(&lower_whole(&module)) {
            Ok(text) => text,
            Err(diagnostics) => {
                failures.push(format!("{call}: {diagnostics:?}"));
                continue;
            }
        };
        let buffer = inkwell::memory_buffer::MemoryBuffer::create_from_memory_range_copy(
            text.as_bytes(),
            "generic-mutation",
        );
        let llvm = context.create_module_from_ir(buffer).unwrap();
        llvm.verify().unwrap();
        let engine = llvm
            .create_jit_execution_engine(inkwell::OptimizationLevel::None)
            .unwrap();
        engine.add_global_mapping(
            &llvm.get_function("record").unwrap(),
            record as *const () as usize,
        );
        RESULT.store(0, std::sync::atomic::Ordering::SeqCst);
        type Main = unsafe extern "C" fn() -> i32;
        let main = unsafe { engine.get_function::<Main>("main").unwrap() };
        assert_eq!(unsafe { main.call() }, 0);
        let result = RESULT.load(std::sync::atomic::Ordering::SeqCst);
        if result != 33 {
            failures.push(format!("{call}: mutation result {result}, expected 33"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn higher_order_functions_accept_parenthesized_mutating_callbacks() {
    static RESULT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    extern "C" fn record(value: i32) {
        RESULT.store(value, std::sync::atomic::Ordering::SeqCst);
    }
    let module = type_check(
        "extern \"c\" { record: I32 -> () }\n\
         def twice: <A> ((mut A -> ()); mut A) -> () = <A> (f; mut a) => { f a; f a }\n\
         def apply: (mut I32 -> ()) -> (mut I32 -> ()) = f => f\n\
         def bump: mut I32 -> () = mut n => { n = n + 1; () }\n\
         let reset: (mut I32 -> ()) = mut n => { n = 1 }\n\
         let mut x: I32 = 5\n\
         reset x\n\
         twice bump x\n\
         twice (apply (mut n => { n = n * 10; () })) x\n\
         record x\n",
    );
    let context = Context::create();
    let text = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .expect("mutating callbacks should compile");
    let buffer = inkwell::memory_buffer::MemoryBuffer::create_from_memory_range_copy(
        text.as_bytes(),
        "mutating-callbacks",
    );
    let llvm = context.create_module_from_ir(buffer).unwrap();
    llvm.verify().unwrap();
    let engine = llvm
        .create_jit_execution_engine(inkwell::OptimizationLevel::None)
        .unwrap();
    engine.add_global_mapping(
        &llvm.get_function("record").unwrap(),
        record as *const () as usize,
    );
    type Main = unsafe extern "C" fn() -> i32;
    let main = unsafe { engine.get_function::<Main>("main").unwrap() };
    assert_eq!(unsafe { main.call() }, 0);
    assert_eq!(RESULT.load(std::sync::atomic::Ordering::SeqCst), 300);
}

#[test]
fn slotted_returned_mutation_closures_drop_non_copy_slots_once() {
    static DROPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    extern "C" fn record(value: i32) {
        let shift = match value {
            1 => 0,
            2 => 8,
            10 => 16,
            20 => 24,
            _ => {
                DROPS.store(u32::MAX, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        DROPS.fetch_add(1 << shift, std::sync::atomic::Ordering::SeqCst);
    }
    let mut failures = Vec::new();
    for (name, body, expected) in [
        ("direct", "update.<Slots> ...local;", 0x01010101),
        (
            "returned",
            "{ let apply = borrow.<Slots> ...local; apply () };",
            0x01010101,
        ),
        (
            "mixed",
            "let (first; second) = local; discard first second;",
            0x00000101,
        ),
    ] {
        let prelude = concat!(
            "extern \"c\" { record: I32 -> () }\n",
            "type Resource wrap I32\n",
            "impl Drop Resource { cleanup = Resource value => record value }\n",
            "type Slots = (Resource; Resource)\n",
            "trait Transform T { transform: T -> T }\n",
            "impl Transform Slots { transform = _ => (Resource 10; Resource 20) }\n",
            "def borrow: <A where Transform A> mut A -> (() -> ()) = <A where Transform A> mut current => () => { current = Transform.transform current; () }\n",
            "def update: <A where Transform A> mut A -> () = <A where Transform A> mut current => { current = Transform.transform current; () }\n",
            "def discard: (move Resource; Resource) -> () = (move first; _) => { drop first; () }\n",
        );
        let module = type_check(&format!(
            "{prelude}def scoped: () ->{{state}} () = () => {{ let mut local: Slots = (Resource 1; Resource 2); {body} () }}\nscoped ()\n"
        ));
        let context = Context::create();
        let text = CodeGenerator::new(&context)
            .compile_module(&lower_whole(&module))
            .unwrap();
        let buffer = inkwell::memory_buffer::MemoryBuffer::create_from_memory_range_copy(
            text.as_bytes(),
            "non-copy-slotted-capture",
        );
        let llvm = context.create_module_from_ir(buffer).unwrap();
        llvm.verify().unwrap();
        let engine = llvm
            .create_jit_execution_engine(inkwell::OptimizationLevel::None)
            .unwrap();
        engine.add_global_mapping(
            &llvm.get_function("record").unwrap(),
            record as *const () as usize,
        );
        DROPS.store(0, std::sync::atomic::Ordering::SeqCst);
        type Main = unsafe extern "C" fn() -> i32;
        let main = unsafe { engine.get_function::<Main>("main").unwrap() };
        assert_eq!(unsafe { main.call() }, 0);
        let drops = DROPS.load(std::sync::atomic::Ordering::SeqCst);
        if drops != expected {
            failures.push(format!(
                "{name}: drop counts {drops:#010x}, expected {expected:#010x}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn slotted_whole_patterns_cannot_hide_mixed_parameter_ownership() {
    for pattern in ["_", "pair"] {
        let source = format!(
            "type Resource wrap I32\nimpl Drop Resource {{ cleanup = Resource _ => () }}\ndef discard: (move Resource; Resource) -> () = {pattern} => ()\n"
        );
        let diagnostics = type_check_errors(
            &source,
            "a whole borrowed pattern cannot consume only one slot",
        );
        assert!(
            diagnostics.iter().any(|message| message.contains("move")),
            "{pattern}: {diagnostics:?}"
        );
    }
}

#[test]
fn slotted_borrowed_closures_cannot_outlive_caller_storage() {
    let diagnostics = type_check_errors(
        concat!(
            "type Slots = (I32; I32)\n",
            "trait Transform T { transform: T -> T }\n",
            "impl Transform Slots { transform = pair => (pair.0 + 10; pair.1 + 20) }\n",
            "def borrow: <A where Transform A> mut A -> (() -> ()) = <A where Transform A> mut current => () => { current = Transform.transform current; () }\n",
            "def invalid: () ->{state} (() -> ()) = () => { let mut local: Slots = (1; 2); borrow.<Slots> ...local }\n",
        ),
        "a caller-owned local cannot be retained by a returned borrowed closure",
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("borrowed closure cannot escape")),
        "{diagnostics:?}"
    );
}

#[test]
fn slotted_function_storage_composition_and_polymorphic_apply_run() {
    static RESULT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    extern "C" fn record(value: i32) {
        RESULT.store(value, std::sync::atomic::Ordering::SeqCst);
    }
    let module = type_check(concat!(
        "extern \"c\" { record: I32 -> () }\n",
        "type Slots = (I32; I32)\n",
        "def add: Slots -> I32 = (a; b) => a + b\n",
        "def twice: I32 -> I32 = value => value * 2\n",
        "def apply: <A, R> (A -> R; A) -> R = <A, R> (f; value) => f value\n",
        "def compose: <A, B, C> (move (A -> B); move (B -> C)) -> (A -> C) = <A, B, C> (move first; move second) => value => second (first value)\n",
        "type Registry wrap (callback: Slots -> I32,)\n",
        "let registry = Registry (callback: add)\n",
        "let mut callbacks: List<Slots -> I32> = List.new ()\n",
        "callbacks |> push add\n",
        "let from_list = callbacks[0]\n",
        "let composed: Slots -> I32 = compose add twice\n",
        "let polymorphic: <A, R> (A -> R; A) -> R = apply\n",
        "let a: I32 = registry.callback 1 2\n",
        "let b: I32 = from_list 3 4\n",
        "let c: I32 = composed 5 6\n",
        "let d: I32 = polymorphic add (7; 8)\n",
        "record (a + b + c + d)\n",
    ));
    let context = Context::create();
    let text = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .unwrap();
    let buffer = inkwell::memory_buffer::MemoryBuffer::create_from_memory_range_copy(
        text.as_bytes(),
        "slotted-function-flow",
    );
    let llvm = context.create_module_from_ir(buffer).unwrap();
    llvm.verify().unwrap();
    let engine = llvm
        .create_jit_execution_engine(inkwell::OptimizationLevel::None)
        .unwrap();
    engine.add_global_mapping(
        &llvm.get_function("record").unwrap(),
        record as *const () as usize,
    );
    type Main = unsafe extern "C" fn() -> i32;
    let main = unsafe { engine.get_function::<Main>("main").unwrap() };
    assert_eq!(unsafe { main.call() }, 0);
    assert_eq!(RESULT.load(std::sync::atomic::Ordering::SeqCst), 47);
}

#[test]
fn slotted_specialized_direct_calls_preserve_the_baseline_abi() {
    let module = type_check(concat!(
        "def pick: <A where Copy A> (A; A) -> A = <A where Copy A> (left; right) => left\n",
        "def invoke: I32 -> I32 = value => pick.<I32> value 22\n",
    ));
    let context = Context::create();
    let text = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .unwrap();
    let caller = text
        .split("define i32 @invoke(ptr %0, i32 %value) {\n")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    let call = caller
        .lines()
        .find(|line| line.contains("call i32 @"))
        .unwrap();
    let callee = call.split('@').nth(1).unwrap().split('(').next().unwrap();
    // Snapshot the pre-change flattened ABI; instance numbering is incidental.
    assert_eq!(
        caller,
        format!(
            "entry:\n  %closure.call = call i32 @{callee}(ptr null, i32 %value, i32 22)\n  ret i32 %closure.call"
        )
    );
    assert!(text.contains(&format!("define internal i32 @{callee}(ptr %0, i32 %left, i32 %right) {{\nentry:\n  ret i32 %left\n}}")));
}

#[test]
fn function_effect_lower_bounds_merge_and_propagate() {
    let module = type_check(concat!(
        "use std.io.(IO, println)\n",
        "type Clock wrap I32\n",
        "def unused = () =>{Clock} 42\n",
        "def empty = () =>{} ()\n",
        "def greet = () =>{Clock} { println \"hello\" }\n",
        "def duplicate = () =>{IO, IO} println \"once\"\n",
        "def forward = () => greet ()\n",
        "def recurse = again: Bool =>{Clock} () :: { if again { recurse False } else { () } }\n",
        "def evaluate: <effect E> (() ->{E} ()) ->{E} () = <effect E> callback => callback ()\n",
        "let stored = greet\n",
        "with Clock = Clock 7 { unused (); forward (); recurse False; evaluate stored }\n",
    ));
    let effects = |name: &str| {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        &module.type_of_function(function.id).unwrap().effects
    };
    assert!(effects("empty").is_empty());
    for name in ["unused", "recurse"] {
        assert_eq!(effects(name).resources.len(), 1);
        assert_eq!(effects(name).resources[0].value_type.to_string(), "Clock");
    }
    for name in ["greet", "forward"] {
        assert_eq!(effects(name).resources.len(), 2);
        assert!(
            effects(name)
                .resources
                .iter()
                .any(|resource| resource.value_type.to_string() == "IO")
        );
        assert!(
            effects(name)
                .resources
                .iter()
                .any(|resource| resource.value_type.to_string() == "Clock")
        );
    }
    assert_eq!(effects("duplicate").resources.len(), 1);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .unwrap();
    assert!(
        llvm.contains("@unused("),
        "unused effect-bearing function should be emitted"
    );
}

#[test]
fn function_effect_lower_bounds_merge_resource_modes_and_state_without_fake_accesses() {
    use staple_compiler::CheckedStateEffect;
    let module = type_check(concat!(
        "type Counter wrap (value: I32)\n",
        "let mut count = 0\n",
        "def update = () =>{Counter} { (resource Counter).value = 1 }\n",
        "def state_union = () =>{state.read} { count = 1 }\n",
        "def unused_state = () =>{state} ()\n",
        "def unused_mut = () =>{mut Counter, Counter} ()\n",
    ));
    let function = |name: &str| {
        module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .unwrap()
    };
    for name in ["update", "unused_mut"] {
        let effects = &module.type_of_function(function(name).id).unwrap().effects;
        assert_eq!(effects.resources.len(), 1);
        assert!(effects.resources[0].mutable, "{name}: {effects}");
    }
    for name in ["state_union", "unused_state"] {
        assert_eq!(
            module
                .type_of_function(function(name).id)
                .unwrap()
                .effects
                .state,
            Some(CheckedStateEffect::ReadWrite)
        );
    }
    let accesses = module
        .state_accesses_of_function(function("state_union").id)
        .unwrap();
    assert!(accesses.reads.is_empty());
    assert_eq!(accesses.writes.len(), 1);
    let accesses = module
        .state_accesses_of_function(function("unused_state").id)
        .unwrap();
    assert!(accesses.reads.is_empty() && accesses.writes.is_empty());
}

#[test]
fn function_effect_lower_bounds_respect_type_and_trait_contracts() {
    for source in [
        "type Clock wrap I32\ndef invalid: () -> () = () =>{Clock} ()\n",
        "trait Pure T { run: T -> () }\nimpl Pure I32 { run = value =>{state} () }\n",
        "type Clock wrap I32\ntrait Pure T { run: T -> () = value =>{Clock} () }\n",
    ] {
        let diagnostics = type_check_errors(source, "lower bounds must fit the contract");
        assert!(
            diagnostics
                .iter()
                .any(|message| message.contains("declared effect set")),
            "{diagnostics:?}"
        );
    }
    type_check("type Clock wrap I32\ndef valid: () ->{Clock, state} () = () =>{Clock} ()\n");
}

#[test]
fn function_effect_lower_bounds_validate_resources_and_variables() {
    for source in [
        "def invalid = () =>{I32} ()\n",
        "def invalid: <effect E, effect F> () ->{E} () = <effect E, effect F> () =>{E, F} ()\n",
    ] {
        let diagnostics = type_check_errors(source, "invalid lower bound");
        assert!(
            diagnostics
                .iter()
                .any(|message| message.contains("concrete, sized nominal type")
                    || message.contains("at most one effect variable")),
            "{diagnostics:?}"
        );
    }
    let diagnostics = type_check_errors(
        "def conflict: <effect E, effect F> (() ->{F} ()) ->{E} () = <effect E, effect F> callback =>{E} callback ()\n",
        "a union of distinct effect variables is not representable",
    );
    assert!(
        diagnostics
            .iter()
            .any(|message| message.contains("distinct effect variables")),
        "{diagnostics:?}"
    );
}

#[test]
fn function_effect_lower_bounds_preserve_open_rows_and_nested_function_purity() {
    let module = type_check(concat!(
        "use std.io.(IO, println)\n",
        "def run: <effect E> (() ->{E} ()) ->{E, IO} () = <effect E> callback =>{E, IO} callback ()\n",
        "def leaf = () =>{IO} println \"hello\"\n",
        "def make = () => () =>{IO} ()\n",
        "run leaf\n",
        "make () ()\n",
    ));
    let make = module
        .functions()
        .iter()
        .find(|function| function.name == "make")
        .unwrap();
    let ty = module.type_of_function(make.id).unwrap();
    assert!(ty.effects.is_empty());
    let CheckedType::Function(inner) = ty.result.as_ref() else {
        panic!("nested function type")
    };
    assert_eq!(inner.effects.resources.len(), 1);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .unwrap();
}

#[test]
fn function_effect_lower_bounds_survive_macro_expansion_and_rendering() {
    let module = type_check(concat!(
        "type Clock wrap I32\n",
        "macro make = resource_type: Type => parse_quote { () =>{$resource_type} 42 }\n",
        "def generated = make Clock\n",
        "with Clock = Clock 7 { generated () }\n",
    ));
    let generated = module
        .functions()
        .iter()
        .find(|function| function.name == "generated")
        .unwrap();
    assert_eq!(
        module
            .type_of_function(generated.id)
            .unwrap()
            .effects
            .resources[0]
            .value_type
            .to_string(),
        "Clock"
    );
    let expanded = staple_compiler::render_expanded_module(module.resolved().program()).unwrap();
    assert!(expanded.contains("=>{Clock}"), "{expanded}");
    staple_syntax::parse(&expanded).unwrap();
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .unwrap();
}

#[test]
fn function_effect_lower_bounds_pass_unused_and_mutable_resources_at_runtime() {
    use std::sync::atomic::{AtomicI32, Ordering};
    static RESULT: AtomicI32 = AtomicI32::new(0);
    extern "C" fn record(value: i32) {
        RESULT.store(value, Ordering::SeqCst);
    }
    let module = type_check(concat!(
        "extern \"c\" { record: I32 -> () }\n",
        "type Clock wrap I32\n",
        "type Counter wrap (value: I32)\n",
        "def unused = () =>{Clock} 42\n",
        "def update = () =>{Counter} { (resource Counter).value = 42 }\n",
        "def read = () =>{Counter} (resource Counter).value\n",
        "let mut counter = Counter (value: 1)\n",
        "with Clock = Clock 7 { with mut Counter = counter { update (); record (unused () + read ()) } }\n",
    ));
    let context = Context::create();
    let text = CodeGenerator::new(&context)
        .compile_module(&lower_whole(&module))
        .unwrap();
    let buffer = inkwell::memory_buffer::MemoryBuffer::create_from_memory_range_copy(
        text.as_bytes(),
        "function-effect-lower-bounds",
    );
    let llvm = context.create_module_from_ir(buffer).unwrap();
    llvm.verify().unwrap();
    let engine = llvm
        .create_jit_execution_engine(inkwell::OptimizationLevel::None)
        .unwrap();
    engine.add_global_mapping(
        &llvm.get_function("record").unwrap(),
        record as *const () as usize,
    );
    type Main = unsafe extern "C" fn() -> i32;
    let main = unsafe { engine.get_function::<Main>("main").unwrap() };
    assert_eq!(unsafe { main.call() }, 0);
    assert_eq!(RESULT.load(Ordering::SeqCst), 84);
}

#[test]
fn function_value_type_parameters_accept_value_only_and_renamed_annotations() {
    let module = type_check(concat!(
        "def identity = <T> move x: T => T :: x\n",
        "def annotated: <T> move T -> T = <U> move x: U => U :: x\n",
        "def drop_value = <T> move x: T => ()\n",
        "let answer: I32 = identity 42\n",
        "let renamed: I32 = annotated 7\n",
        "let unit: () = drop_value 1\n",
    ));
    lower(&module);
}

#[test]
fn function_value_type_parameters_reject_mismatched_contracts() {
    for (source, message) in [
        ("def f: <T> move T -> T = move x => x", "declares none"),
        (
            "def f: <T> move T -> T = <U, V> move x: U => U :: x",
            "count, kind, or sizedness",
        ),
        (
            "def f: <T where Copy T> T -> T = <U> x: U => U :: x",
            "bounds differ",
        ),
        (
            "def f: I32 -> I32 = <T> x: I32 => x",
            "has no compile-time parameters",
        ),
        ("def f = <T> x => x", "needs a type annotation"),
    ] {
        let errors = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("invalid value generics");
        assert!(
            errors.iter().any(|error| error.message.contains(message)),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn function_value_type_parameters_isolate_annotation_names() {
    let errors = resolve_result("def f: <T> move T -> T = <U> move x: T => T :: x")
        .expect_err_diagnostics("annotation names must not leak");
    assert!(errors.iter().any(|error| error.message.contains("T")));
}

#[test]
fn function_value_type_parameters_cover_contexts_and_generic_impls() {
    type_check(concat!(
        "trait Visit T { visit: <A> (T, move A) -> A }\n",
        "impl Visit I32 { visit = <B> (_, move value: B) => value }\n",
        "type Box<T> wrap T\n",
        "impl <T> Visit Box<T> { visit = <B> (_, move value: B) => { let local: B = value; local } }\n",
        "trait DefaultVisit T { visit_default: <A> (T, move A) -> A = <B> (_, move value: B) => value }\n",
        "impl DefaultVisit I32 {}\n",
        "def consume = f: (<T> move T -> T) => I32 :: f 7\n",
        "let contextual = consume (<U> move x: U => x)\n",
        "let ascribed = (<T> move T -> T) :: <U> move x: U => x\n",
        "def apply = <effect E> f: (() ->{E} I32) =>{E} I32 :: f ()\n",
        "let answer = apply (() => 42)\n",
        "let member_result = Visit.visit (Box 1, 2)\n",
    ));
}

#[test]
fn function_value_type_parameters_require_recursive_results() {
    type_check("def recurse = <T> move x: T => T :: recurse x\n");
    let diagnostics = TypeChecker::new()
        .check(resolve("def recurse = <T> move x: T => recurse x\n"))
        .expect_err_diagnostics("recursive results must be written");
    assert!(
        diagnostics.iter().any(|d| d
            .message
            .contains("recursive generic function value must ascribe")),
        "{diagnostics:?}"
    );
}

#[test]
fn function_value_type_parameters_check_kinds_sizedness_and_extra_bounds() {
    const SHAPE: &str =
        "function value compile-time parameters differ in count, kind, or sizedness";
    for (source, expected) in [
        (
            "def f: <T> move T -> T = <effect E> x: I32 => I32 :: x",
            SHAPE,
        ),
        (
            "use std.shared.(Shared)\ndef f: <T where ?Sized T> Shared<T> -> Shared<T> = <U> x: Shared<U> => x",
            SHAPE,
        ),
        (
            "def f: <T> T -> T = <U where Copy U> x: U => x",
            "function value compile-time parameter bounds differ from the function type",
        ),
        (
            "def f: <T> T -> T = x => x",
            "the function type declares compile-time parameters `<T>`, but the function value \
             declares none",
        ),
        (
            "trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = (_, move value) => value }",
            "trait member `Visit.visit` must implement scheme",
        ),
        (
            "trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = <B, C> (_, move value: B) => value }",
            SHAPE,
        ),
        (
            "trait Show T { show: T -> I32 }\nimpl Show I32 { show = <B> x: I32 => I32 :: 1 }",
            "trait member `Show.show` is not generic: the function type has no compile-time \
             parameters",
        ),
        (
            "trait Show T { show: T -> I32 = <B> x: T => I32 :: 2 }\nimpl Show I32 {}",
            "trait member `Show.show` is not generic: the function type has no compile-time \
             parameters",
        ),
        (
            "def f: I32 -> I32 = <T> x: I32 => x",
            "the function type has no compile-time parameters, but the function value declares \
             parameters",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("generic contracts must match");
        assert!(
            diagnostics.iter().any(|d| d.message.contains(expected)),
            "{source}: expected `{expected}` in {diagnostics:?}"
        );
    }
    // The hint names the annotation's parameters and adds no `move` of its own.
    let diagnostics = TypeChecker::new()
        .check(resolve("def f: <T> T -> T = x => x"))
        .expect_err_diagnostics("missing value parameters");
    assert!(
        diagnostics.iter().all(|d| !d.message.contains("move")),
        "{diagnostics:?}"
    );
}

#[test]
fn function_value_type_parameters_reject_unknown_subtype_bound_parameters() {
    for source in [
        "def f = <T where U <: I32> move x: T => x\n",
        "def f: <T where U <: I32> move T -> T = <V> move x: V => x\n",
    ] {
        let diagnostics = resolve_result(source)
            .err()
            .expect("unknown subtype-bound parameter");
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("unknown compile-time parameter `U` in subtype bound")),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn closures_over_mutable_locals_keep_contextual_state_effects() {
    // Captured-state effects stay contextual until effect inference, so these
    // pre-existing programs keep checking.
    type_check(concat!(
        "def counter: I32 -> (() -> I32) = start => {\n",
        "    let mut total = start\n",
        "    () => total\n",
        "}\n",
        "def generic_counter = <T where Copy T> start: T => (() -> T) :: {\n",
        "    let mut total = start\n",
        "    () => total\n",
        "}\n",
        "def call: (() -> I32) -> I32 = f => f ()\n",
        "def run = () => {\n",
        "    let mut total = 3\n",
        "    let g = () => total\n",
        "    call g\n",
        "}\n",
    ));
}

#[test]
fn function_value_type_parameters_infer_and_merge_effects() {
    let module = type_check(concat!(
        "use std.io.(IO, println)\n",
        "def apply = <effect E> f: (() ->{E} ()) => f ()\n",
        "def greet = <effect E> f: (() ->{E} ()) =>{E} { f (); println \"hello\" }\n",
        "apply (() => println \"apply\")\n",
        "greet (() => ())\n",
        "let stored_apply = apply\n",
        "let stored_greet = greet\n",
        "let alias = stored_greet\n",
        "stored_apply (() => println \"stored apply\")\n",
        "stored_greet (() => ())\n",
        "alias (() => println \"callback\")\n",
    ));
    for name in ["apply", "greet"] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        let ty = module.type_of_function(function.id).unwrap();
        assert!(ty.effects.variable.is_some(), "{name}: {}", ty.effects);
        assert_eq!(ty.effects.resources.len(), usize::from(name == "greet"));
        let scheme = module.scheme_of_function(function.id).unwrap();
        let CheckedType::Function(body) = scheme.body.as_ref() else {
            panic!("function scheme")
        };
        assert_eq!(body.effects, ty.effects);
    }
    lower(&module);
}

#[test]
fn function_value_type_parameters_recheck_stored_effect_contracts() {
    let source = concat!(
        "use std.io.(IO, println)\n",
        "def greet = <effect E> f: (() ->{E} ()) =>{E} { f (); println \"hello\" }\n",
        "let bad: <effect F> (() ->{F} ()) ->{F} () = greet\n",
        "bad (() => ())\n",
    );
    let diagnostics = TypeChecker::default()
        .check(resolve(source))
        .expect_err_diagnostics("inferred IO must not disappear through an explicit scheme");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("expected") && d.message.contains("IO")),
        "{diagnostics:?}"
    );
    let monomorphic = source.replace("<effect F> (() ->{F} ()) ->{F} ()", "(() -> ()) -> ()");
    let diagnostics = TypeChecker::default()
        .check(resolve(&monomorphic))
        .expect_err_diagnostics("inferred IO must not disappear through a monomorphic annotation");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("expected") && d.message.contains("IO")),
        "{diagnostics:?}"
    );
    let valid = source.replace("->{F} () = greet", "->{F, IO} () = greet");
    let module = type_check(&valid);
    lower(&module);
    let indirect = valid
        .replace("def greet", "def say = () => println \"hello\"\ndef greet")
        .replace("{ f (); println \"hello\" }", "{ f (); say () }");
    let module = type_check(&indirect);
    lower(&module);
}

#[test]
fn function_value_type_parameters_survive_macro_freshening() {
    let module = type_check(concat!(
        "macro identity = _: Expr => parse_quote { <T> move x: T => T :: x }\n",
        "let first = identity ()\n",
        "let second = identity ()\n",
        "let a: I32 = first 42\n",
        "let b: String = second \"hello\"\n",
    ));
    lower(&module);
}

#[test]
fn annotated_nominal_patterns_supply_parameter_types() {
    let module = type_check(concat!(
        "type Box<T> wrap T\n",
        "def first = ((Box inner): Box<I32>; index: USize) => inner\n",
        "def moved = move (Box inner): Box<String> => String :: inner\n",
        "def unbox = <T> move (Box inner): Box<T> => T :: inner\n",
        "def matched = (value: Box<I32>) => match value { (Box n): Box<I32> => n }\n",
        "let (Box local): Box<I32> = Box 7\n",
        "let total: I32 = first (Box 1) 2 + unbox (Box 3) + matched (Box 4) + local\n",
        "let text: String = moved (Box \"s\")\n",
    ));
    lower(&module);
    for (source, expected) in [
        (
            "type Box<T> wrap T\ndef f = ((Box inner): I32) => inner\n",
            "nominal pattern `Box` cannot match `I32`",
        ),
        (
            "type Box<T> wrap T\nlet (Box local): Box<I32> = Box \"text\"\n",
            "expected",
        ),
        (
            "type Box<T> wrap T\ndef f: Box<I32> -> I32 = ((Box inner): Box<String>) => inner\n",
            "Box<String>",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("mismatched nominal annotation");
        assert!(
            diagnostics.iter().any(|d| d.message.contains(expected)),
            "{source}: expected `{expected}` in {diagnostics:?}"
        );
    }
}

#[test]
fn juxtaposed_calls_fill_compile_time_helper_slots() {
    // A slotted parameter on a compile-time helper takes one juxtaposed
    // argument per slot, the same as a runtime function. Unnamed slotted
    // products used to collapse into a single macro shape and reject the call.
    let module = type_check(concat!(
        "def pair: (Expr; Expr) -> Expr = (left; right) => parse_quote { ($left, $right) }\n",
        "def named: (left: Expr; right: Expr) -> Expr = (left; right) => parse_quote { ($left, $right) }\n",
        "def triple: (Expr; Expr; Expr) -> Expr = (first; second; third) => parse_quote { ($first, $second, $third) }\n",
        "def mixed: (Expr; I32) -> Expr = (value; count) => parse_quote { $value }\n",
        "def plain: (Expr, Expr) -> Expr = (left, right) => parse_quote { ($left, $right) }\n",
        "def inferred = (left: Expr; right: Expr) => Expr :: parse_quote { ($left, $right) }\n",
        "macro pair_value: Expr -> Expr = value => pair value value\n",
        "macro named_value: Expr -> Expr = value => named value value\n",
        "macro triple_value: Expr -> Expr = value => triple value value value\n",
        "macro mixed_value: Expr -> Expr = value => mixed value 2\n",
        "macro plain_value: Expr -> Expr = value => plain (value, value)\n",
        "macro inferred_value: Expr -> Expr = value => inferred value value\n",
        "macro partial: Expr -> Expr = value => {\n",
        "    let apply: Expr -> Expr = pair value\n",
        "    apply value\n",
        "}\n",
        "let paired: (I32, I32) = pair_value 1\n",
        "let named_pair: (I32, I32) = named_value 2\n",
        "let tripled: (I32, I32, I32) = triple_value 3\n",
        "let mixed_value_: I32 = mixed_value 4\n",
        "let plain_pair: (I32, I32) = plain_value 5\n",
        "let inferred_pair: (I32, I32) = inferred_value 6\n",
        "let partial_pair: (I32, I32) = partial 7\n",
    ));
    lower(&module);
}

#[test]
fn a_slotted_argument_can_fill_one_nested_product_slot() {
    // `(Expr; Expr)` may be the first slot's own type rather than a whole
    // two-slot argument, so the call must still bind it to that one slot.
    let module = type_check(concat!(
        "def count: ((Expr; Expr); I32) -> I32 = (pair; count) => count\n",
        "macro read_count: Expr -> Expr = value => {\n",
        "    let number: I32 = count (value; value) 2\n",
        "    let _ = number\n",
        "    value\n",
        "}\n",
        "let result: I32 = read_count 1\n",
    ));
    lower(&module);
}

#[test]
fn compile_time_helpers_accept_whole_slotted_products() {
    let module = type_check(concat!(
        "def pair: (left: Expr; right: Expr) -> Expr = (left; right) => parse_quote { ($left, $right) }\n",
        "macro literal: Expr -> Expr = value => pair (value; value)\n",
        "macro binding: Expr -> Expr = value => {\n",
        "    let arguments: (left: Expr; right: Expr) = (value; value)\n",
        "    pair arguments\n",
        "}\n",
        "let from_literal: (I32, I32) = literal 1\n",
        "let from_binding: (I32, I32) = binding 2\n",
    ));
    lower(&module);
}

#[test]
fn value_defined_macro_helpers_supply_their_signature() {
    // A helper's parameter annotations and result ascription stand in for a
    // binding annotation, including `parse_quote`'s contextual syntax type.
    let module = type_check(concat!(
        "def increment = value: Expr => Expr :: parse_quote { $value + 1 }\n",
        "def add = left: Expr => right: Expr => Expr :: parse_quote { $left + $right }\n",
        "def pair = (first: Expr, second: Expr) => Expr :: parse_quote { ($first, $second) }\n",
        "macro inc: Expr -> Expr = value => increment value\n",
        "macro sum: Expr -> Expr = value => add value value\n",
        "macro both: Expr -> Expr = value => pair (value, value)\n",
        "let answer: I32 = inc 41\n",
        "let doubled: I32 = sum 21\n",
        "let twice: (I32, I32) = both 7\n",
    ));
    lower(&module);
}

#[test]
fn value_defined_companion_members_use_companion_parameters() {
    let module = type_check(concat!(
        "type Box<T> wrap T\n",
        "companion<T> Box<T> {\n",
        "    pub def unwrap = move (Box value): Box<T> => T :: value\n",
        "    pub def rewrap = <U> (move box: Box<T>, move f: (move T -> U)) => Box<U> :: {\n",
        "        let Box value = box\n",
        "        Box (f value)\n",
        "    }\n",
        "}\n",
        "def name = move value: I32 => String :: \"one\"\n",
        "let answer: I32 = Box.unwrap (Box 42)\n",
        "let text: Box<String> = Box.rewrap (Box 1, name)\n",
    ));
    lower(&module);
}

#[test]
fn raw_pointer_memory_intrinsics_require_copy_and_matching_types() {
    for (original, replacement) in [
        (
            "__pointer_load: <T where Copy T> RawPtr<T> -> T",
            "__pointer_load: <T> RawPtr<T> -> T",
        ),
        (
            "__mut_pointer_load: <T where Copy T> RawMutPtr<T> -> T",
            "__mut_pointer_load: <T where Copy T> RawMutPtr<T> -> I32",
        ),
        (
            "__pointer_store: <T where Copy T> (RawMutPtr<T>, T) -> ()",
            "__pointer_store: <T> (RawMutPtr<T>, T) -> ()",
        ),
        (
            "__pointer_store: <T where Copy T> (RawMutPtr<T>, T) -> ()",
            "__pointer_store: <T where Copy T> (RawPtr<T>, T) -> ()",
        ),
    ] {
        let diagnostics = stdlib_modifier_diagnostics("std/ptr.sta", original, replacement);
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("standard-library intrinsic has an invalid type")),
            "{replacement}: {diagnostics:?}"
        );
    }
}

#[test]
fn unique_constructs_dereferences_and_transfers_raw_ownership() {
    let module = type_check(concat!(
        "use std.unique.(Unique)\n",
        "use std.ptr.(RawMutPtr)\n",
        "def roundtrip = () => {\n",
        "  let value = Unique (x: 12, y: 30)\n",
        "  let pointer = Unique.unsafe_into_raw value\n",
        "  let mut restored = Unique.unsafe_from_raw pointer\n",
        "  restored.x = restored.x + 1\n",
        "  restored.y\n",
        "}\n",
        "def generic = <T where ?Sized T> move value: Unique<T> => Unique<T> :: Unique.unsafe_from_raw (Unique.unsafe_into_raw value)\n",
        "def unbox = () => { let Unique value = Unique 42; value }\n",
        "def deref = () => { let value = Unique 42; value.*; value.* }\n",
        "def borrowed_copy = value: Unique<I32> => I32 :: value.*\n",
        "def constructor_value = () => { let allocate: I32 -> Unique<I32> = Unique; let owner = allocate 9; owner.* }\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("Unique should emit");
    assert!(llvm.contains("@malloc"));
    assert!(llvm.contains("@free"));
}

#[test]
fn unique_is_affine_and_borrowed_payloads_cannot_move() {
    for source in [
        "use std.unique.(Unique)\ndef bad = () => { let value = Unique 1; let other = value; value }\n",
        "use std.unique.(Unique)\ndef bad = (value: Unique<I32>) => Unique.unsafe_into_raw value\n",
        "use std.unique.(Unique)\nuse std.cinterop.*\ndef bad = (value: Unique<CString>) => value.*\n",
        "use std.unique.(Unique)\nuse std.cinterop.*\ndef bad = () => { let mut owner = Unique (c_string \"owned\"); owner.* = owner.*; () }\n",
    ] {
        let errors = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("Unique ownership must be enforced");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("moved value")
                    || error.message.contains("borrowed value")),
            "{errors:?}"
        );
    }
}

#[test]
fn unique_and_raw_pointers_accept_unsized_pointees() {
    let module = type_check(concat!(
        "use std.unique.(Unique)\n",
        "use std.ptr.*\n",
        "type Unsized opaque\n",
        "def adopt = pointer: RawMutPtr<Unsized> => Unique<Unsized> :: Unique.unsafe_from_raw pointer\n",
        "def release = move value: Unique<Unsized> => RawMutPtr<Unsized> :: Unique.unsafe_into_raw value\n",
        "def erase = pointer: RawPtr<Unsized> => RawOpaquePtr :: Convert.convert pointer\n",
        "def cast = pointer: RawPtr<Unsized> => RawMutPtr<Unsized> :: Convert.convert pointer\n",
        "def offset = pointer: RawPtr<Unsized> => RawPtr<Unsized> :: RawPtr.offset pointer 0\n",
        "def mutable_offset = pointer: RawMutPtr<Unsized> => RawMutPtr<Unsized> :: RawMutPtr.offset pointer 0\n",
        "def nullable = () => RawPtr<Unsized> :: RawPtr.null ()\n",
        "def nullable_mut = () => RawMutPtr<Unsized> :: RawMutPtr.null ()\n",
        "def generic = <T where ?Sized T> pointer: RawMutPtr<T> => RawPtr<T> :: Convert.convert pointer\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("unsized pointer handles should emit without a pointee layout");
    let dynamic = Lowerer::new()
        .with_dictionary_pass_generic_calls(true)
        .lower(&module)
        .expect("unsized handles should lower with generic dictionary passing enabled");
    CodeGenerator::new(&context)
        .compile_root(&dynamic)
        .expect("unsized handles should emit with generic dictionary passing enabled");
    for source in [
        "use std.unique.(Unique)\nuse std.ptr.*\ndef invalid = move value: Unique<I32> => RawMutPtr<I32> :: Convert.convert value\n",
        "use std.unique.(Unique)\nuse std.ptr.*\ndef invalid = value: RawMutPtr<I32> => Unique<I32> :: Convert.convert value\n",
    ] {
        TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("Unique has no Convert implementation");
    }
}

#[test]
fn unique_guards_recursive_types_and_borrows_non_copy_payloads() {
    type_check(concat!(
        "use std.unique.(Unique)\n",
        "type Node<T> wrap (head: T, tail: Option<Unique<Node<T>>>)\n",
        "def make: () -> Node<I32> = () => Node.<I32> (head: 42, tail: None)\n",
        "def nest: () -> Node<I32> = () => { let leaf = make (); Node.<I32> (head: 1, tail: Some (Unique leaf)) }\n",
        "def read: Unique<I32> -> I32 = Unique value => value\n",
    ));
    let errors = TypeChecker::new()
        .check(resolve(concat!(
            "use std.unique.(Unique)\n",
            "use std.cinterop.*\n",
            "def invalid: Unique<CString> -> CString = Unique value => value\n",
        )))
        .expect_err_diagnostics("borrowed destructuring cannot move a payload");
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("borrowed value")),
        "{errors:?}"
    );
}

#[test]
fn unique_builtin_cleanup_cannot_be_overridden() {
    let errors = TypeChecker::new()
        .check(resolve(
            "use std.unique.(Unique)\nimpl Drop Unique<I32> { cleanup = owner => () }\n",
        ))
        .expect_err_diagnostics("Unique owns its built-in cleanup");
    assert!(
        errors.iter().any(|error| error
            .message
            .contains("Drop` may only be implemented for a represented nominal type")),
        "{errors:?}"
    );
}

#[test]
fn macros_expand_inside_string_interpolation() {
    let module = type_check(concat!(
        "use std.source.(line)\n",
        "macro five = () => parse_quote { 5 }\n",
        "let message = \"a ${five ()} b ${line ()}\"\n",
    ));
    let value = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == "message" => binding.value.clone(),
            _ => None,
        })
        .expect("binding `message` should exist");
    let staple_syntax::Expression::StringTemplate(template) = value else {
        panic!("expected a string template");
    };
    let literals = template
        .parts
        .iter()
        .filter_map(|part| match part {
            staple_syntax::StringTemplatePart::Interpolation(interpolation) => {
                match &*interpolation.expression {
                    staple_syntax::Expression::Integer(integer) => Some(integer.literal.clone()),
                    other => panic!("expected an integer literal, found {other:?}"),
                }
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(literals, ["5", "3"]);
}

#[test]
fn track_caller_location_api_resolves_and_type_checks() {
    type_check(concat!(
        "use std.source.Location\n",
        "let here = Location.caller ()\n",
        "let text = \"${here}\"\n",
        "let file = Location.file here\n",
        "let line = Location.line here\n",
        "let column = Location.column here\n",
    ));
}

#[test]
fn macro_quotes_resolve_traits_their_defining_module_imports() {
    // `for` expands to `IntoIterator.into_iterator` and `Iterator.next`,
    // which `std.flow` imports. Without the prelude the expanding module
    // names neither trait, so they must resolve where `for` is defined.
    type_check(concat!(
        "@no_prelude\npub mod\n",
        "let mut total: I32 = 0\n",
        "for item in (0 .. 3) { total = total + item }\n",
    ));
}

#[test]
fn companions_see_traits_their_parent_module_declares() {
    type_check(concat!(
        "trait Show T { show: T -> I32 }\n",
        "impl Show I32 { show = value => value }\n",
        "type Box wrap I32\n",
        "companion Box {\n",
        "    pub def via_trait = <T where Show T> value: T => I32 :: Show.show value\n",
        "}\n",
        "let shown = Box.via_trait 3\n",
    ));
}

#[test]
fn companions_see_their_defining_modules_imports() {
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Box wrap I32\n",
        "companion Box {\n",
        "    pub def share = (box: Box) => Shared<Box> :: Shared box\n",
        "}\n",
        "let shared: Shared<Box> = Box.share (Box 1)\n",
    ));
    // A block may import a name its defining module already imports.
    type_check(concat!(
        "use std.shared.(Shared)\n",
        "type Box wrap I32\n",
        "companion Box {\n",
        "    use std.shared.(Shared)\n",
        "    pub def share = (box: Box) => Shared<Box> :: Shared box\n",
        "}\n",
    ));
}

#[test]
fn companion_block_imports_reach_no_other_block() {
    let diagnostics = resolve_result(concat!(
        "type Box wrap I32\n",
        "companion Box {\n",
        "    use std.shared.(Shared)\n",
        "    pub def share = (box: Box) => Shared<Box> :: Shared box\n",
        "}\n",
        "companion Box {\n",
        "    pub def again = (box: Box) => Shared<Box> :: Shared box\n",
        "}\n",
    ))
    .expect_err_diagnostics("another block's import must not be visible");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown type `Shared`")),
        "{diagnostics:?}"
    );
    // Each block may import the same name for itself.
    type_check(concat!(
        "type Box wrap I32\n",
        "companion Box {\n",
        "    use std.shared.(Shared)\n",
        "    pub def share = (box: Box) => Shared<Box> :: Shared box\n",
        "}\n",
        "companion Box {\n",
        "    use std.shared.(Shared)\n",
        "    pub def again = (box: Box) => Shared<Box> :: Shared box\n",
        "}\n",
    ));
}

#[test]
fn companion_blocks_share_one_namespace() {
    // Every block's members are the type's, but a block reaches another
    // block's members through the type, like any other module.
    type_check(concat!(
        "type Box wrap I32\n",
        "companion Box { pub def one = (box: Box) => I32 :: 1 }\n",
        "companion Box { pub def two = (box: Box) => I32 :: Box.one box + (box |> one) }\n",
        "let total: I32 = Box.one (Box 1) + Box.two (Box 1)\n",
    ));
    let diagnostics = resolve_result(concat!(
        "type Box wrap I32\n",
        "companion Box { pub def one = (box: Box) => I32 :: 1 }\n",
        "companion Box { pub def two = (box: Box) => I32 :: one box }\n",
    ))
    .expect_err_diagnostics("a block does not see another block's members directly");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown name `one`"),
        "{diagnostics:?}"
    );
    type_check(concat!(
        "use std.syntax.*\n",
        "pub type Box<T> wrap pub (value: T)\n",
        "companion<T> Box<T> { pub def label = (box: Box<T>) => I32 :: 1 }\n",
        "companion<T> Box<T> {\n",
        "    pub macro of = value: Expr => parse_quote { Box (value: $value) }\n",
        "}\n",
        "let label: I32 = Box.label (Box.of 5)\n",
    ));
    let diagnostics = resolve_result(concat!(
        "type Box wrap I32\n",
        "companion Box { pub def one = (box: Box) => I32 :: 1 }\n",
        "companion Box { def one = (box: Box) => I32 :: 2 }\n",
    ))
    .expect_err_diagnostics("a member may be declared by only one block");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("duplicate definition of `one`")),
        "{diagnostics:?}"
    );
}

#[test]
fn track_caller_flags_only_the_outer_stage() {
    let resolved = resolve(concat!(
        "use std.source.(track_caller)\n@track_caller\n",
        "def outer = a: I32 => b: I32 => a + b\n",
        "def plain = a: I32 => a\n",
    ));
    let tracked = |name: &str| {
        let functions = resolved
            .functions()
            .iter()
            .filter(|function| function.name.ends_with(name))
            .collect::<Vec<_>>();
        assert!(!functions.is_empty(), "`{name}` should resolve");
        functions
    };
    let outer = tracked("outer");
    assert_eq!(outer.len(), 1);
    assert!(outer[0].track_caller);
    assert!(resolved.is_track_caller(outer[0].id));
    assert!(!resolved.is_track_caller(tracked("plain")[0].id));
    // The curried inner stage and every other function stay untracked, apart
    // from the standard library's `Location.caller`, `panic`, the
    // `Try` blanket companion's `unwrap` and `expect`, and the number module's
    // `Divide`/`Remainder` implementations and float `clamp`.
    assert_eq!(
        resolved
            .functions()
            .iter()
            .filter(|function| {
                resolved.is_track_caller(function.id)
                    && !function.name.ends_with("caller")
                    && !function.name.ends_with("panic")
                    && !function.name.contains("std.try.")
                    && !function.name.contains("std.number.")
            })
            .count(),
        1,
    );
    let std_source = resolve("use std.source.Location\nlet here = Location.caller ()\n");
    assert!(
        std_source
            .functions()
            .iter()
            .any(|function| function.track_caller && function.name.ends_with("caller")),
        "Location.caller should be a track_caller function"
    );
}

#[test]
fn track_caller_is_accepted_on_module_companion_and_generic_defs() {
    for source in [
        "use std.source.(track_caller)\n@track_caller\npub def here = () => 1\n",
        "use std.source.(track_caller)\n@track_caller\ndef identity = <T> value: T => value\n",
        "use std.source.(track_caller)\n@track_caller\ndef identity: <T> T -> T = value => value\n",
        "use std.source.(track_caller)\ntype Box wrap I32\ncompanion Box {\n    use std.source.(track_caller)\n    @track_caller\n    pub def make = () => Box 1\n}\n",
        "use std.source.(track_caller)\ntype Box wrap I32\ncompanion Box {\n    use std.source.(track_caller)\n    /// Documented.\n    @track_caller\n    pub def make = () => Box 1\n}\n",
        "use std.source.(track_caller)\ntype Box<T> wrap T\ncompanion<T> Box<T> {\n    use std.source.(track_caller)\n    /// Documented.\n    @track_caller\n    pub def get = (box: Box<T>) => box\n}\n",
        "use std.source.(track_caller)\n/// Documented.\n@track_caller\ndef documented = () => 1\n",
        "use std.source.(track_caller)\n@track_caller\ndef recursive = n: I32 => if (n == 0) { 0 } else { recursive (n - 1) }\n",
    ] {
        let resolved = resolve_result(source)
            .unwrap_or_else(|diagnostics| panic!("{source:?} should resolve: {diagnostics:?}"));
        assert!(
            resolved.functions().iter().any(|function| function.track_caller),
            "{source:?} should flag a function"
        );
    }
    type_check("use std.source.(track_caller)\n@track_caller\ndef identity = <T> move value: T => value\nlet x = identity 1\n");
}

#[test]
fn track_caller_rejects_unsupported_placements() {
    for (source, expected) in [
        (
            "use std.source.(track_caller)\n@track_caller\nlet value = () => 1\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\n@track_caller\nconst value = 1\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\n@track_caller\ndef value = 1\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\n@track_caller\ndef value: () -> I32\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\n@track_caller\nextern \"C\" {\n    puts: () -> I32\n}\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\n@track_caller\ntype Token wrap I32\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\n@track_caller\ntrait Named T { name: T -> I32 }\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\nuse std.cmp.Eq\n@track_caller\nimpl Eq I32 { eq = (a; b) => True }\n",
            "`@track_caller` may only modify a `def` whose value is a function",
        ),
        (
            "use std.source.(track_caller)\n@track_caller(1)\ndef value = () => 1\n",
            "`@track_caller`",
        ),
        (
            "use std.source.(track_caller)\ndef outer = () => {\n    @track_caller\n    def inner = () => 1\n    inner ()\n}\n",
            "not a local one",
        ),
        (
            "use std.source.(track_caller)\ndef outer = x: I32 => {\n    @track_caller\n    def inner = () => x\n    inner ()\n}\n",
            "not a local one",
        ),
        (
            "use std.source.(track_caller)\nlet value = {\n    @track_caller\n    def inner = () => 1\n    inner ()\n}\n",
            "not a local one",
        ),
    ] {
        let diagnostics = resolve_result(source)
            .expect_err_diagnostics(&format!("{source:?} should be rejected"));
        assert!(
            diagnostics.iter().any(|d| d.message.contains(expected)),
            "{source:?}: {diagnostics:?}"
        );
    }
}

#[test]
fn track_caller_allows_reading_module_level_values() {
    resolve("use std.source.(track_caller)\nlet captured = 1\n@track_caller\ndef uses_global = () => captured\n");
}

#[test]
fn track_caller_modifier_is_stdlib_reserved() {
    let diagnostics = resolve_result(&with_syntax_imports(
        "macro @track_caller: Item -> Item = item => item\n",
    ))
    .expect_err_diagnostics("track_caller name is reserved");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("reserved by the compiler")),
        "{diagnostics:?}"
    );
}

#[test]
#[test]
fn track_caller_requires_an_import_from_std_source() {
    for source in [
        "@track_caller\ndef value = () => 1\n",
        "use std.core.*\n@track_caller\ndef value = () => 1\n",
        "use std.annotation.(track_caller)\n@track_caller\ndef value = () => 1\n",
    ] {
        resolve_result(source).expect_err_diagnostics("track_caller needs an import");
    }
    for source in [
        "use std.source.(track_caller)\n@track_caller\ndef value = () => 1\n",
        "use std.source.*\n@track_caller\ndef value = () => 1\n",
        "@std.source.track_caller\ndef value = () => 1\n",
    ] {
        let resolved = resolve_result(source)
            .unwrap_or_else(|diagnostics| panic!("{source:?}: {diagnostics:?}"));
        assert!(resolved.functions().iter().any(|f| f.track_caller));
    }
}

fn caller_intrinsics_require_a_track_caller_body() {
    let program = "use std.source.Location\nlet here = Location.caller ()\n";
    // Baseline: the unmodified standard library is accepted.
    let diagnostics = stdlib_edit_diagnostics(
        "std/source.sta",
        "@track_caller\n    pub def caller",
        "@track_caller\n    pub def caller",
        program,
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    for (original, replacement) in [
        // Not a track_caller function at all.
        ("@track_caller\n    pub def caller", "pub def caller"),
        // Inside a nested closure rather than the function's own body.
        (
            "file: __caller_file (),",
            "file: (() => __caller_file ()) (),",
        ),
    ] {
        let diagnostics = stdlib_edit_diagnostics("std/source.sta", original, replacement, program);
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("may only be used directly in the body of a `@track_caller` function")),
            "{original:?} -> {replacement:?}: {diagnostics:?}"
        );
    }
    // A `coro` body runs from its own frame, not the function's environment.
    let diagnostics = stdlib_edit_diagnostics(
        "std/source.sta",
        "file: __caller_file (),",
        "file: coro { __caller_file () },",
        program,
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("cannot be used inside a `coro` body")),
        "{diagnostics:?}"
    );
}

#[test]
fn caller_location_span_maps_macro_generated_references_to_the_invocation() {
    let resolved = resolve(concat!(
        "use std.source.(track_caller)\n",
        "@track_caller\n",
        "def tracked = () => 1\n",
        "macro call = () => parse_quote { tracked () }\n",
        "let direct = tracked ()\n",
        "let generated =\n",
        "    call ()\n",
    ));
    let callee = |name: &str| {
        let value = resolved
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => binding.value.clone(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("binding `{name}` should exist"));
        match value {
            staple_syntax::Expression::Call(call) => call.callee.syntax().id,
            other => panic!("expected a call, found {other:?}"),
        }
    };
    // A reference written in source reports its own span.
    assert!(resolved.caller_location_span(callee("direct")).is_none());
    // A reference produced by the macro reports the outermost invocation.
    let span = resolved
        .caller_location_span(callee("generated"))
        .expect("generated reference should be mapped");
    let position = resolved
        .program()
        .span_position(span)
        .expect("invocation span should have a position");
    assert_eq!((position.line, position.column), (7, 5));
    assert_eq!(position.file, "<stdin>.sta");
}

#[test]
fn span_position_agrees_with_source_location_macros() {
    let module = type_check(concat!(
        "use std.source.(line, column, file)\n",
        "let at = (line (), column (), file ())\n",
    ));
    let binding_span = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == "at" => {
                Some(binding.value.as_ref().unwrap().syntax().span.clone())
            }
            _ => None,
        })
        .unwrap();
    let position = module.resolved().program().span_position(&binding_span).unwrap();
    assert_eq!(position.file, "<stdin>.sta");
    assert_eq!(position.line, 2);
}

/// The program's source after macro expansion, whitespace removed.
fn expanded_compact(source: &str) -> String {
    let resolved = resolve(source);
    staple_compiler::render_expanded_module(resolved.program())
        .unwrap()
        .split_whitespace()
        .collect()
}

const PARSE_IMPORTS: &str = "use std.syntax.(Syntax, Expr, Type, Pattern, Item, SyntaxNode, Comma, Equals, Ident, Sequence, Separated, Parenthesized, Bracketed, Braced, Parse, ParseError, ParseStream, stream, is_empty, remaining, error_at, quote, parse_quote)\n";

#[test]
fn parse_reads_streaming_leaves_in_order() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro leaves = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let mut input = stream contents\n",
            "    let Ok first = Parse.parse.<Expr> input\n",
            "    let Ok comma = Parse.parse.<Comma> input\n",
            "    let Ok last = Parse.parse.<Ident<String>> input\n",
            "    if (is_empty input) { parse_quote { ($first, $last) } } else { panic \"stream should be empty\" }\n",
            "}\n",
            "let a = 1\n",
            "let b = 2\n",
            "let c = 10\n",
            "let result: (I32, I32) = leaves { a + b, c }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("(a+b,c)"), "{compact}");
    type_check(&source);
}

#[test]
fn parse_all_dispatches_from_context_and_from_a_type_argument() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro contextual = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let parsed: Result<Expr, ParseError> = Parse.parse_all contents\n",
            "    match parsed {\n",
            "        Ok value => parse_quote { ($value, 1) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro explicit = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<Parenthesized<Separated<Ident<String>, Comma>>> contents) {\n",
            "        Ok Parenthesized (Separated (_, elements, _)) => parse_quote { 7 },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "let one: (I32, I32) = contextual { 1 + 2 }\n",
            "let two: I32 = explicit { (alpha, beta, gamma) }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("(1+2,1)"), "{compact}");
    type_check(&source);
}

#[test]
fn parse_all_rejects_leftover_syntax_with_a_position() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro single = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<Ident<String>> contents) {\n",
            "        Ok _ => parse_quote { 1 },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "let value: I32 = single { alpha , beta }\n",
        )
    );
    let diagnostics = resolve_result(&source).expect_err_diagnostics("leftover syntax");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .starts_with("macro `single` panicked: unexpected syntax at line")),
        "{diagnostics:#?}"
    );
}

#[test]
fn parse_reads_slotted_and_comma_products() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro binding = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<(Ident<\"set\">; Ident<String>; Equals; Expr)> contents) {\n",
            "        Ok (_, name, _, value) => parse_quote { ($name, $value) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro pair = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<(Expr, Expr)> contents) {\n",
            "        Ok (left, right) => parse_quote { ($right, $left) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro nothing = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<()> contents) {\n",
            "        Ok _ => parse_quote { 0 },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "let x = 5\n",
            "let a = 1\n",
            "let b = 2\n",
            "let bound: (I32, I32) = binding { set x = 1 + 2 }\n",
            "let none: I32 = nothing {  }\n",
            "let plain: (I32, I32) = pair { a, b }\n",
            "let trailing: (I32, I32) = pair { a, b, }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("(x,1+2)"), "{compact}");
    assert!(compact.contains("letplain:(I32,I32)=(b,a)"), "{compact}");
    assert!(compact.contains("lettrailing:(I32,I32)=(b,a)"), "{compact}");
    type_check(&source);
}

#[test]
fn comma_products_reject_adjacent_elements_and_doubled_trailing_commas() {
    for (body, expected) in [
        ("a b", "expected Comma"),
        ("a, b,,", "unexpected syntax"),
        ("a", "expected Ident<String>"),
    ] {
        let source = format!(
            "{PARSE_IMPORTS}{}",
            format!(
                concat!(
                    "macro pair = body: Braced<Syntax> => {{\n",
                    "    let Braced (contents) = body\n",
                    "    match (Parse.parse_all.<(Ident<String>, Ident<String>)> contents) {{\n",
                    "        Ok _ => parse_quote {{ 0 }},\n",
                    "        ParseError (message) => panic message,\n",
                    "    }}\n",
                    "}}\n",
                    "let value: I32 = pair {{ {} }}\n",
                ),
                body
            )
        );
        let diagnostics = resolve_result(&source).expect_err_diagnostics(body);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .starts_with(&format!("macro `pair` panicked: {expected}"))),
            "{body}: {diagnostics:#?}"
        );
    }
}

#[test]
fn parse_reads_sequences_options_and_separated_bodies() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro atoms = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<Sequence<SyntaxNode>> contents) {\n",
            "        Ok Sequence (first, rest) => parse_quote { ($first, 1) },\n",
            "        Ok Sequence () => parse_quote { (0, 0) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro maybe = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<(Option<Type>; Equals; Expr)> contents) {\n",
            "        Ok (Some _, _, value) => parse_quote { ($value, 1) },\n",
            "        Ok (None, _, value) => parse_quote { ($value, 0) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro fields = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<Braced<Separated<(Ident<String>; Equals; Expr), Comma>>> contents) {\n",
            "        Ok Braced (Separated (_, Sequence (first, rest), _)) => {\n",
            "            let (_, _, value) = first\n",
            "            parse_quote { ($value, 2) }\n",
            "        },\n",
            "        Ok _ => parse_quote { (0, 0) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "let a = 3\n",
            "let first: (I32, I32) = atoms { a (b c) d }\n",
            "let empty: (I32, I32) = atoms { }\n",
            "let value = 4\n",
            "let typed: (I32, I32) = maybe { = value }\n",
            "let annotated: (I32, I32) = maybe { I32 = value }\n",
            "let field: (I32, I32) = fields { { x = a, y = 2, } }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("letfirst:(I32,I32)=(a,1)"), "{compact}");
    assert!(compact.contains("letempty:(I32,I32)=(0,0)"), "{compact}");
    assert!(compact.contains("lettyped:(I32,I32)=(value,0)"), "{compact}");
    assert!(compact.contains("letannotated:(I32,I32)=(value,1)"), "{compact}");
    assert!(compact.contains("letfield:(I32,I32)=(a,2)"), "{compact}");
    type_check(&source);
}

#[test]
fn parse_forks_backtrack_and_commit() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro forks = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let mut input = stream contents\n",
            "    let mut fork = input\n",
            "    let Ok _ = Parse.parse.<Expr> fork\n",
            "    let unmoved = if (is_empty input) { parse_quote { 0 } } else { parse_quote { 1 } }\n",
            "    let moved = if (is_empty fork) { parse_quote { 1 } } else { parse_quote { 0 } }\n",
            "    input = fork\n",
            "    let committed = if (is_empty input) { parse_quote { 1 } } else { parse_quote { 0 } }\n",
            "    parse_quote { ($unmoved, $moved, $committed) }\n",
            "}\n",
            "let flags: (I32, I32, I32) = forks { value }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("letflags:(I32,I32,I32)=(1,1,1)"), "{compact}");
    type_check(&source);
}

#[test]
fn a_failed_optional_leaves_the_stream_unmoved() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro unmoved = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let mut input = stream contents\n",
            "    let Ok skipped = Parse.parse.<Option<Type>> input\n",
            "    let Ok equals = Parse.parse.<Equals> input\n",
            "    let Ok value = Parse.parse.<Expr> input\n",
            "    parse_quote { ($value, 1) }\n",
            "}\n",
            "let value = 9\n",
            "let result: (I32, I32) = unmoved { = value }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("letresult:(I32,I32)=(value,1)"), "{compact}");
    type_check(&source);
}

#[test]
fn mut_parameters_of_compile_time_helpers_update_the_callers_stream() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "def skip_one = mut input: ParseStream => () :: {\n",
            "    let Ok _ = Parse.parse.<Ident<String>> input\n",
            "}\n",
            "macro second = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let mut input = stream contents\n",
            "    skip_one input\n",
            "    let Ok name = Parse.parse.<Ident<String>> input\n",
            "    parse_quote { ($name, 2) }\n",
            "}\n",
            "let beta = 1\n",
            "let result: (I32, I32) = second { alpha beta }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("letresult:(I32,I32)=(beta,2)"), "{compact}");
    type_check(&source);
}

#[test]
fn passing_an_immutable_binding_to_a_mut_parameter_is_rejected() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "def skip_one = mut input: ParseStream => () :: {\n",
            "    let Ok _ = Parse.parse.<Ident<String>> input\n",
            "}\n",
            "macro second = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let input = stream contents\n",
            "    skip_one input\n",
            "    let Ok name = Parse.parse.<Ident<String>> input\n",
            "    parse_quote { ($name, 2) }\n",
            "}\n",
        )
    );
    let diagnostics = resolve_result(&source).expect_err_diagnostics("immutable mut argument");
    let count = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message == "cannot write through `input`; its binding is not declared `mut`"
        })
        .count();
    assert_eq!(count, 2, "{diagnostics:#?}");
}

#[test]
fn question_mark_and_return_leave_the_innermost_compile_time_function() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "def name_or_fail = mut input: ParseStream => Result<Ident<String>, ParseError> :: {\n",
            "    let seen = {\n",
            "        let ident = Parse.parse.<Ident<String>> input?\n",
            "        ident\n",
            "    }\n",
            "    Ok seen\n",
            "}\n",
            "def early = value: Expr => Expr :: {\n",
            "    let ignored = { return value }\n",
            "    parse_quote { 0 }\n",
            "}\n",
            "macro succeeds = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let mut input = stream contents\n",
            "    match (name_or_fail input) {\n",
            "        Ok name => parse_quote { ($name, 1) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro fails = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    let mut input = stream contents\n",
            "    match (name_or_fail input) {\n",
            "        Ok name => parse_quote { ($name, 1) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro returns = value: Expr => early value\n",
            "let alpha = 1\n",
            "let good: (I32, I32) = succeeds { alpha }\n",
            "let early_value: I32 = returns alpha\n",
            "let bad: (I32, I32) = fails { 1 + 2 }\n",
        )
    );
    let diagnostics = resolve_result(&source).expect_err_diagnostics("failed ? exits early");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .starts_with("macro `fails` panicked: expected Ident<String>")),
        "{diagnostics:#?}"
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
}

const KEY_VALUE_PARSE: &str = concat!(
    "pub type KeyValue wrap pub (key: Ident<String>, value: Expr)\n",
    "impl Parse KeyValue {\n",
    "    parse = mut input => {\n",
    "        let key = Parse.parse.<Ident<String>> input?\n",
    "        let equals = Parse.parse.<Equals> input?\n",
    "        let value = Parse.parse.<Expr> input?\n",
    "        Ok (KeyValue (key: key, value: value))\n",
    "    }\n",
    "}\n",
);

#[test]
fn user_impls_parse_custom_nodes_in_macros_and_helpers() {
    let source = format!(
        "{PARSE_IMPORTS}{KEY_VALUE_PARSE}{}",
        concat!(
            "pub type Pair<T> wrap pub (first: T, second: T)\n",
            "impl <T where Parse T> Parse Pair<T> {\n",
            "    parse = mut input => {\n",
            "        let first = Parse.parse.<T> input?\n",
            "        let comma = Parse.parse.<Comma> input?\n",
            "        let second = Parse.parse.<T> input?\n",
            "        Ok (Pair (first: first, second: second))\n",
            "    }\n",
            "}\n",
            "def entry = body: Syntax => Result<KeyValue, ParseError> :: Parse.parse_all body\n",
            "macro field = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (entry contents) {\n",
            "        Ok KeyValue (key, value) => parse_quote { ($key, $value) },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro pair_of = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<Pair<KeyValue>> contents) {\n",
            "        Ok Pair (left, right) => {\n",
            "            let KeyValue (_, first) = left\n",
            "            let KeyValue (_, second) = right\n",
            "            parse_quote { ($first, $second) }\n",
            "        },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "macro broken = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (entry contents) {\n",
            "        Ok _ => parse_quote { 0 },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "let key = 1\n",
            "let a = 2\n",
            "let b = 3\n",
            "let single: (I32, I32) = field { key = a + 1 }\n",
            "let both: (I32, I32) = pair_of { x = a, y = b }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("letsingle:(I32,I32)=(key,a+1)"), "{compact}");
    assert!(compact.contains("letboth:(I32,I32)=(a,b)"), "{compact}");
    // The implementations and their intrinsics exist only while expanding.
    assert!(!compact.contains("implParse"), "{compact}");
    let module = type_check(&source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_root(&lower(&module))
        .expect("programs with `Parse` implementations should compile");

    let broken = format!("{source}let bad: I32 = broken {{ key a }}\n");
    let diagnostics = resolve_result(&broken).expect_err_diagnostics("a failing user parse");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .starts_with("macro `broken` panicked: expected Equals at line")),
        "{diagnostics:#?}"
    );
}

#[test]
fn parse_diagnostics_name_the_missing_context_target_and_implementation() {
    let cases: [(&str, &str, &str); 4] = [
        (
            "    let parsed = Parse.parse_all contents\n    parse_quote { 0 }\n",
            "",
            "`Parse.parse` requires a contextual `Result<T, ParseError>` type or an explicit type argument",
        ),
        (
            "    let parsed: Result<Unknown, ParseError> = Parse.parse_all contents\n    parse_quote { 0 }\n",
            "",
            "`Unknown` does not implement `Parse`",
        ),
        (
            "    match (Parse.parse_all.<Expr> contents) {\n        Ok value => parse_quote { 0 },\n    }\n",
            "",
            "non-exhaustive match",
        ),
        (
            "    match (Parse.parse_all.<Expr> contents) {\n        Ok value => parse_quote { 0 },\n        ParseError (message) => panic message,\n    }\n",
            "impl Parse I32 { parse = mut input => Ok 1 }\n",
            "cannot implement an external trait for external types; the trait or at least one top-level nominal implementation argument must be defined in the current package",
        ),
    ];
    for (body, extra, expected) in cases {
        let source = format!(
            "{PARSE_IMPORTS}{extra}macro m = body: Braced<Syntax> => {{\n    let Braced (contents) = body\n{body}}}\n"
        );
        let diagnostics = resolve_result(&source).expect_err_diagnostics(expected);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.message.contains(expected)),
            "{expected}: {diagnostics:#?}"
        );
    }
}

#[test]
fn ambiguous_user_parse_impls_are_reported_at_the_use() {
    let source = format!(
        "{PARSE_IMPORTS}{KEY_VALUE_PARSE}{}{}",
        "impl Parse KeyValue { parse = mut input => Ok (KeyValue (key: Ident \"x\", value: Expr \"y\")) }\n",
        concat!(
            "macro m = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<KeyValue> contents) {\n",
            "        Ok _ => parse_quote { 0 },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
        )
    );
    let diagnostics = resolve_result(&source).expect_err_diagnostics("ambiguous impls");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message.contains(
            "`KeyValue` has ambiguous `Parse` implementations: `impl Parse KeyValue` and `impl Parse KeyValue`"
        )),
        "{diagnostics:#?}"
    );
}

#[test]
fn parsed_caller_identifiers_keep_their_hygiene() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "macro shadow = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<Ident<String>> contents) {\n",
            "        Ok name => parse_quote { { let a = 100\n a; $name } },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "let a = \"caller\"\n",
            "let result: String = shadow { a }\n",
        )
    );
    type_check(&source);
}

#[test]
fn parse_names_resolve_through_namespaces_globs_and_renames() {
    let namespaced = concat!(
        "use std.syntax\n",
        "use std.syntax.(Braced, Syntax, Ident, ParseError, parse_quote)\n",
        "macro m = body: Braced<Syntax> => {\n",
        "    let Braced (contents) = body\n",
        "    let mut input = syntax.stream contents\n",
        "    match (syntax.Parse.parse.<Ident<String>> input) {\n",
        "        Ok name => parse_quote { ($name, 1) },\n",
        "        ParseError (message) => panic message,\n",
        "    }\n",
        "}\n",
        "let alpha = 1\n",
        "let result: (I32, I32) = m { alpha }\n",
    );
    assert!(expanded_compact(namespaced).contains("letresult:(I32,I32)=(alpha,1)"));
    let renamed = concat!(
        "use std.syntax.*\n",
        "use std.syntax.(stream as open_stream, Parse as Reader)\n",
        "macro m = body: Braced<Syntax> => {\n",
        "    let Braced (contents) = body\n",
        "    let mut input = open_stream contents\n",
        "    match (Reader.parse.<Ident<String>> input) {\n",
        "        Ok name => parse_quote { ($name, 1) },\n",
        "        ParseError (message) => panic message,\n",
        "    }\n",
        "}\n",
        "macro bare = body: Braced<Syntax> => {\n",
        "    let Braced (contents) = body\n",
        "    match (parse_all.<Ident<String>> contents) {\n",
        "        Ok name => parse_quote { ($name, 2) },\n",
        "        ParseError (message) => panic message,\n",
        "    }\n",
        "}\n",
        "let alpha = 1\n",
        "let first: (I32, I32) = m { alpha }\n",
        "let second: (I32, I32) = bare { alpha }\n",
    );
    let compact = expanded_compact(renamed);
    assert!(compact.contains("letfirst:(I32,I32)=(alpha,1)"), "{compact}");
    assert!(compact.contains("letsecond:(I32,I32)=(alpha,2)"), "{compact}");
}

#[test]
fn if_and_when_work_in_parse_members() {
    let source = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "pub type Tagged wrap pub (name: Ident<String>, extra: Option<Ident<String>>)\n",
            "impl Parse Tagged {\n",
            "    parse = mut input => {\n",
            "        let name = Parse.parse.<Ident<String>> input?\n",
            "        when {\n",
            "            (is_empty input) => Ok (Tagged (name: name, extra: None)),\n",
            "            else => match (Parse.parse.<Comma> input) {\n",
            "                Ok comma => match (Parse.parse.<Ident<String>> input) {\n",
            "                    Ok extra => Ok (Tagged (name: name, extra: Some extra)),\n",
            "                    failure => failure,\n",
            "                },\n",
            "                failure => failure,\n",
            "            },\n",
            "        }\n",
            "    }\n",
            "    parse_all = syntax => {\n",
            "        let mut input = stream syntax\n",
            "        let value = Parse.parse.<Tagged> input?\n",
            "        if (is_empty input) { Ok value } else { error_at (input; \"trailing syntax\") }\n",
            "    }\n",
            "}\n",
            "macro tagged = body: Braced<Syntax> => {\n",
            "    let Braced (contents) = body\n",
            "    match (Parse.parse_all.<Tagged> contents) {\n",
            "        Ok Tagged (name, extra) => match extra {\n",
            "            None => parse_quote { ($name, 1) },\n",
            "            Some other => parse_quote { ($other, 2) },\n",
            "        },\n",
            "        ParseError (message) => panic message,\n",
            "    }\n",
            "}\n",
            "let alpha = 1\n",
            "let beta = 2\n",
            "let one: (I32, I32) = tagged { alpha }\n",
            "let two: (I32, I32) = tagged { alpha, beta }\n",
        )
    );
    let compact = expanded_compact(&source);
    assert!(compact.contains("letone:(I32,I32)=(alpha,1)"), "{compact}");
    assert!(compact.contains("lettwo:(I32,I32)=(beta,2)"), "{compact}");
    type_check(&source);
}

#[test]
fn the_macros_example_with_parse_type_checks() {
    type_check(include_str!("../examples/macros.sta"));
}

#[test]
fn parse_impl_members_are_checked_against_the_trait() {
    let unbounded = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "pub type Wrapper<T> wrap pub T\n",
            "impl <T> Parse Wrapper<T> {\n",
            "    parse = mut input => {\n",
            "        let inner = Parse.parse.<T> input?\n",
            "        Ok (Wrapper inner)\n",
            "    }\n",
            "}\n",
        )
    );
    let diagnostics = resolve_result(&unbounded).expect_err_diagnostics("a missing bound");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message
            == "`T` does not implement `Parse`; add a `Parse T` bound"),
        "{diagnostics:#?}"
    );
    let immutable = format!(
        "{PARSE_IMPORTS}{}",
        concat!(
            "pub type Marker wrap pub ()\n",
            "impl Parse Marker { parse = input => Ok (Marker ()) }\n",
        )
    );
    let diagnostics = resolve_result(&immutable).expect_err_diagnostics("a copied stream");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message
            == "a `Parse.parse` implementation must declare its parameter `mut`"),
        "{diagnostics:#?}"
    );
}

#[test]
fn associated_type_equalities_unify_iterator_items() {
    let module = type_check(
        r#"
        def join = <L, R where Iterator L, Iterator R, Iterator.Item L == Iterator.Item R>
            (move left: L; move right: R) => left |> chain right
        def test = () => {
            let items = (0 .. 3) |> join ((3 .. 6) |> filter (x => x > 3)) |> to_list
            items |> into_iterator |> fold 0 ((a, b) => a + b)
        }
        let total = test ()
    "#,
    );
    lower(&module);
}

#[test]
fn a_trait_member_may_fix_a_trait_parameter_through_its_own_equality() {
    // `insert_all` mentions `Item` only in `Iterator.Item Iter == Item`. The
    // equality places `Item` in the member's own bound, which a use solves
    // from `Iter`, so the member is accepted, with a default body and with an
    // implementation's override.
    let module = type_check(
        r#"
        use std.iterator.IterStep.(Done, Yield)
        trait Bulk Target Item {
            insert: (move Target; move Item) -> Target
            insert_all: <Iter where Iterator Iter, Iterator.Item Iter == Item>
                (move Target; move Iter) -> Target =
                <Iter where Iterator Iter, Iterator.Item Iter == Item> (move target; move iter) => {
                    let mut built = target
                    let mut remaining = iter
                    loop {
                        match Iterator.next remaining {
                            Done _ => { break },
                            Yield (item, rest) => { remaining = rest; built = insert built item },
                        }
                    }
                    built
                }
        }
        type Count wrap USize
        impl Bulk Count I32 { insert = (move Count total; move _item) => Count (total + 1) }
        type Last wrap I32
        impl Bulk Last I32 {
            insert = (move _last; move item) => Last item
            insert_all = <Iter where Iterator Iter, Iterator.Item Iter == I32> (move last; move _iter) => last
        }
        let counted = Bulk.insert_all (Count 0) (1 .. 5)
        let kept = Bulk.insert_all (Last 7) (1 .. 5)
    "#,
    );
    lower(&module);
    let errors = type_check_errors(
        "trait Unfixed Target Item { make: () -> Target }\n",
        "a member that fixes `Item` nowhere is still rejected",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("must mention trait parameter `Item`")),
        "{errors:?}"
    );
}

#[test]
fn bounds_on_a_generic_item_type_are_checked_in_generic_code() {
    // Regression test: a bound whose argument is the enclosing function's own
    // associated type was treated as unsolved and skipped, so a generic
    // caller could omit it.
    let errors = type_check_errors(
        concat!(
            "use std.ops.(Add)\n",
            "def needs = <Iter where Iterator Iter, Add (Iterator.Item Iter)> move iter: Iter => () :: ()\n",
            "def user = <Iter where Iterator Iter> move iter: Iter => () :: needs iter\n",
        ),
        "`user` does not provide `Add (Iterator.Item Iter)`",
    );
    assert!(
        errors
            .iter()
            .any(|error| error == "trait bound is not satisfied for `Iterator.Item`"),
        "{errors:?}"
    );
    let module = type_check(concat!(
        "use std.ops.(Add)\n",
        "def needs = <Iter where Iterator Iter, Add (Iterator.Item Iter)> move iter: Iter => () :: ()\n",
        "def user = <Iter where Iterator Iter, Add (Iterator.Item Iter)> move iter: Iter => () :: needs iter\n",
        "let used = user (0 .. 3)\n",
    ));
    lower(&module);
}

#[test]
fn explicit_underscores_complete_collect_through_equality_bounds() {
    let module = type_check(
        r#"
        use std.list.List
        def run = () => {
            let items = (0 .. 3) |> collect.<_, List<I32>, _>
            items |> into_iterator |> fold 0 ((a, b) => a + b)
        }
        let total = run ()
    "#,
    );
    lower(&module);
}

#[test]
fn projection_arguments_complete_in_ordinary_trait_bounds() {
    let module = type_check(
        r#"
        use std.list.List
        use std.iterator.IterStep.(Done, Yield)

        def gather = <Iter, C where Iterator Iter, FromIterator C (Iterator.Item Iter)>
            (move iter: Iter; move seed: C) => C :: {
            let mut result = seed
            let mut remaining = iter
            loop {
                match Iterator.next remaining {
                    Done _ => { break },
                    Yield (item, rest) => {
                        remaining = rest
                        result = FromIterator.insert result item
                    },
                }
            }
            result
        }

        def run = () => {
            let seed: List<I32> = List.new ()
            let items = gather (0 .. 3) seed
            items |> into_iterator |> fold 0 ((a, b) => a + b)
        }
        let total = run ()
    "#,
    );
    lower(&module);
}

#[test]
fn associated_type_equalities_reject_mismatching_items() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            r#"
        let items = (0 .. 3) |> chain (String.bytes "hi")
    "#,
        ))
        .expect_err_diagnostics("different iterator items must fail");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("associated-type equality")
                && d.message.contains("Iterator.Item")),
        "{diagnostics:?}"
    );
}

#[test]
fn associated_type_equalities_support_concrete_generic_and_transitive_targets() {
    let module = type_check(
        r#"
        trait Element T { type Value; value: T -> Value }
        impl Element I32 { type Value = I32; value = x => x }
        impl Element Bool { type Value = I32; value = x => 7 }
        def integer = <T where Element.Value T == I32, Element T> x: T => I32 :: Element.value x
        def generic = <T, A where Element T, A == Element.Value T> x: T => A :: Element.value x
        def transitive = <T, U, A where Element T, Element U,
            Element.Value T == A, Element.Value U == Element.Value T>
            (x: T; y: U) => (A, A) :: (Element.value x, Element.value y)
        def forwarded = <T where Element T, Element.Value T == I32> x: T => integer x
        let flag: Bool = True
        let a: I32 = integer flag
        let b: I32 = generic 3
        let c: (I32, I32) = transitive 3 flag
        let d: I32 = forwarded flag
        let f: <T where Element T, Element.Value T == I32> T -> I32 =
            <T where Element T, Element.Value T == I32> x => Element.value x
        let e: I32 = f flag
        let getter: <T, A where Element T, Element.Value T == A> T -> A =
            <T, A where Element T, Element.Value T == A> x => Element.value x
        let recovered: I32 = getter flag
    "#,
    );
    lower(&module);
}

#[test]
fn associated_type_equalities_reject_invalid_requirements() {
    for source in [
        r#"def bad = <T where Iterator T, Iterator.Item T == I32, Iterator.Item T == String> move x: T => ()"#,
        r#"def bad = <T where Iterator.Item T == I32> move x: T => ()"#,
        r#"def bad = <T where T == I32> x: T => x"#,
        r#"def bad = <T where Iterator T, Iterator.Item T == List<Iterator.Item T>> move x: T => ()"#,
        r#"def only_int = <T where Iterator T, Iterator.Item T == I32> move x: T => x |> count
           let bad = only_int (String.bytes "hi")"#,
        r#"def same = <T, A, B where Iterator T, Iterator.Item T == A, Iterator.Item T == B> move x: T => x |> count
           let bad = same.<Range<I32>, I32, String> (0 .. 2)"#,
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics("invalid associated equality must fail");
        assert!(!diagnostics.is_empty(), "{source}");
    }
}

#[test]
fn associated_type_equalities_constrain_types_and_implementations() {
    let module = type_check(
        r#"
        trait Element T { type Value; value: T -> Value }
        impl Element I32 { type Value = I32; value = x => x }
        impl Element Bool { type Value = String; value = x => "text" }
        type Integers<T where Element T, Element.Value T == I32> wrap T
        trait Read T { read: T -> I32 }
        impl<T where Element T, Element.Value T == I32> Read Integers<T> {
            read = Integers x => Element.value x
        }
        let wrapped = Integers.<I32> 3
        let value: I32 = Read.read wrapped
    "#,
    );
    lower(&module);
    let diagnostics = TypeChecker::new()
        .check(resolve(
            r#"
        type Integers<T where Iterator T, Iterator.Item T == I32> wrap T
        let bad = Integers.<Range<U8>> (0 .. 2)
    "#,
        ))
        .expect_err_diagnostics("type constructor must enforce equality");
    assert!(!diagnostics.is_empty());
}

#[test]
fn associated_type_equalities_infer_parameters_used_only_in_bounds() {
    let module = type_check(
        r#"
        def count_as = <Iter, A where Iterator Iter, Iterator.Item Iter == A>
            move iter: Iter => USize :: iter |> count
        def count_as_twice = <Iter, A, B where Iterator Iter,
            Iterator.Item Iter == A, Iterator.Item Iter == B>
            move iter: Iter => USize :: iter |> count
        let a = count_as (0 .. 3)
        let b = count_as_twice (0 .. 4)
    "#,
    );
    lower(&module);
}

#[test]
fn associated_type_equalities_support_nested_projections_and_prerequisites() {
    let module = type_check(
        r#"
        def integer_source = <Source where IntoIterator Source,
            Iterator.Item IntoIterator.Iter Source == I32>
            move source: Source => I32 :: source |> into_iterator |> sum
        trait IntSource Source {
            type Iter where Iterator Iter, Iterator.Item Iter == I32
            iter: move Source -> Iter
        }
        type Source wrap I32
        impl IntSource Source {
            type Iter = Range<I32>
            iter = move Source n => 0 .. n
        }
        def source_sum = <S where IntSource S> move source: S => I32 :: IntSource.iter source |> sum
        let a = integer_source (0 .. 4)
        let b = source_sum (Source 4)
    "#,
    );
    lower(&module);
}

#[test]
fn associated_type_equalities_in_companions_and_generic_implementations() {
    let module = type_check(
        r#"
        type Box<T> wrap T
        companion<T where Iterator T, Iterator.Item T == I32> Box<T> {
            def total = move boxed: Box<T> => I32 :: match boxed { Box iter => iter |> sum }
        }
        trait Total T { total: move T -> I32 }
        impl<T where Iterator T, Iterator.Item T == I32> Total Box<T> {
            total = move boxed => match boxed { Box iter => iter |> sum }
        }
        let a = Box.<Range<I32>> (0 .. 4) |> Box.total
        let b = Total.total (Box.<Range<I32>> (0 .. 4))
    "#,
    );
    lower(&module);
    let diagnostics = TypeChecker::new()
        .check(resolve(
            r#"
        type Box<T> wrap T
        trait Total T { total: move T -> I32 }
        impl<T where Iterator T, Iterator.Item T == I32> Total Box<T> {
            total = move boxed => match boxed { Box iter => iter |> sum }
        }
        let bad = Total.total (Box.<Range<U8>> (0 .. 3))
    "#,
        ))
        .expect_err_diagnostics("conditional implementation equality must hold");
    assert!(!diagnostics.is_empty());
}

#[test]
fn associated_type_equalities_do_not_make_overlapping_implementations_disjoint() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            r#"
        type Box<T> wrap T
        trait Total T { total: move T -> I32 }
        impl<T where Iterator T, Iterator.Item T == I32> Total Box<T> {
            total = move boxed => match boxed { Box iter => iter |> sum }
        }
        impl<T, A where Iterator T, Iterator.Item T == A> Total Box<T> {
            total = move Box iter => 0
        }
    "#,
        ))
        .expect_err_diagnostics("unresolved equality does not disprove overlap");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("duplicate trait implementation")),
        "{diagnostics:?}"
    );
}

#[test]
fn associated_type_equalities_are_checked_without_items_in_the_result() {
    for source in [
        r#"def same = <L, R where Iterator L, Iterator R, Iterator.Item L == Iterator.Item R>
                (move l: L; move r: R) => ()
            let bad = same (0 .. 3) (String.bytes "hi")"#,
        r#"type Pair<L, R where Iterator L, Iterator R, Iterator.Item L == Iterator.Item R> wrap (L, R)
            let bad = Pair.<Range<I32>, Range<U8>> (0 .. 3, 0 .. 3)"#,
        r#"def same = <I where Iterator I, Iterator.Item I == _> move i: I => ()"#,
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics(source);
        assert!(!diagnostics.is_empty(), "{source}");
    }
    let module = type_check(
        r#"
        type Pair<L, R where Iterator L, Iterator R, Iterator.Item L == Iterator.Item R> wrap (L, R)
        let pair = Pair.<Range<I32>, Map<Range<I32>, I32>> (0 .. 3, (0 .. 3) |> map (x => x + 1))
    "#,
    );
    lower(&module);
}

#[test]
fn associated_type_equalities_compare_compound_types_exactly() {
    for source in [
        r#"def bad = <T where Iterator T, Iterator.Item T == (left: I32, flag: Bool), Iterator.Item T == (right: I32, flag: Bool)> move x: T => ()"#,
        r#"use std.io.IO
           def bad = <T where Iterator T, Iterator.Item T == (I32 -> I32), Iterator.Item T == (I32 ->{IO} I32)> move x: T => ()"#,
        r#"def bad = <T where Iterator T, Iterator.Item T == (move I32 -> I32), Iterator.Item T == (I32 -> I32)> move x: T => ()"#,
        r#"def bad = <T where Iterator T, Iterator.Item T == (I32 | String), Iterator.Item T == (I32 | Bool)> move x: T => ()"#,
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err_diagnostics(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("equality") && d.message.contains("cannot hold")),
            "{diagnostics:?}"
        );
    }
    let module = type_check(
        r#"
        def valid = <T where Iterator T, Iterator.Item T == (I32 | String),
            Iterator.Item T == (String | I32)> move x: T => ()
    "#,
    );
    lower(&module);
}

#[test]
fn associated_type_equalities_constrain_implementation_item_pairs() {
    let module = type_check(
        r#"
        type Pair<L, R> wrap (L, R)
        trait Amount T { amount: move T -> USize }
        impl<L, R where Iterator L, Iterator R, Iterator.Item L == Iterator.Item R> Amount Pair<L, R> {
            amount = move Pair (l, r) => (l |> count) + (r |> count)
        }
        let amount = Amount.amount (Pair.<Range<I32>, Map<Range<I32>, I32>> (0 .. 2, (0 .. 2) |> map (x => x * 2)))
    "#,
    );
    lower(&module);
    let diagnostics = TypeChecker::new().check(resolve(r#"
        type Pair<L, R> wrap (L, R)
        trait Amount T { amount: move T -> USize }
        impl<L, R where Iterator L, Iterator R, Iterator.Item L == Iterator.Item R> Amount Pair<L, R> {
            amount = move Pair (l, r) => (l |> count) + (r |> count)
        }
        let bad = Amount.amount (Pair.<Range<I32>, Range<U8>> (0 .. 2, 0 .. 2))
    "#)).expect_err_diagnostics("implementation item types must agree");
    assert!(!diagnostics.is_empty());
}

#[test]
fn associated_type_equalities_do_not_infer_iterator_types_from_items() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            r#"
        def choose = <I where Iterator I, Iterator.Item I == I32> () => ()
        let bad = choose ()
    "#,
        ))
        .expect_err_diagnostics("an associated output must not select the iterator type");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("could not infer")),
        "{diagnostics:?}"
    );
}
