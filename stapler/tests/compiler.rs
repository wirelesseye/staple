use inkwell::context::Context;
use stapler::{
    CheckedMutation, CheckedType, CodeGenerator, Item, NameResolver, ProgramLoader,
    RecursiveConstruction, TypeChecker, parse,
};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn resolve(source: &str) -> stapler::ResolvedModule {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = with_syntax_imports(source);
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(&source, root)
        .expect("source should load");
    NameResolver::new()
        .resolve_program(program)
        .expect("source should resolve")
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
                || source.contains("Sequence Item"),
        ),
        (
            "Syntax",
            source.contains(": Syntax")
                || source.contains("-> Syntax")
                || source.contains("Braced Syntax"),
        ),
        ("SyntaxNode", source.contains("SyntaxNode")),
        ("Ident", source.contains("Ident")),
        ("CallExpr", source.contains("CallExpr")),
        ("Sequence", source.contains("Sequence")),
        ("Optional", source.contains("Optional")),
        ("Separated", source.contains("Separated")),
        ("Comma", source.contains("Comma")),
        ("Equals", source.contains("Equals")),
        ("FatArrow", source.contains("FatArrow")),
        ("Parenthesized", source.contains("Parenthesized")),
        ("Bracketed", source.contains("Bracketed")),
        ("Braced", source.contains("Braced")),
        (
            "MacroCallVisibility",
            source.contains("MacroCallVisibility"),
        ),
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

fn type_check(source: &str) -> stapler::TypedModule {
    TypeChecker::new()
        .check(resolve(source))
        .expect("source should type-check")
}

#[test]
fn implicitly_thunks_call_arguments_and_preserves_callback_effects() {
    let module = type_check(concat!(
        "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
        "let mut count = 0\n",
        "let first = evaluate { count = count + 1; count }\n",
        "let second = evaluate count\n",
    ));

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("implicit thunks should lower as ordinary callbacks");
    assert!(llvm.contains("implicit_thunk"));
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
        .compile_module(&module)
        .expect("signals and reactions should lower");
    assert!(llvm.contains("__staple_signal_track"));
    assert!(llvm.contains("__staple_signal_notify"));
    assert!(llvm.contains("__staple_reaction_create"));
    assert!(llvm.contains("__staple_reactive_scope_dispose"));
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
        .compile_module(&module)
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
        .expect_err("effectful derived initializers should be rejected");
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
fn implicitly_thunks_fixed_product_argument_positions() {
    let module = type_check(concat!(
        "def add_later: (I32, () -> I32) -> I32 = (left, right) => left + right ()\n",
        "let implicit = add_later (40, 2)\n",
        "let direct = add_later (40, () => 2)\n",
    ));

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("fixed product slots should lower implicit thunks independently");
}

#[test]
fn implicit_thunking_is_not_a_general_value_coercion() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let callback: () -> I32 = 42\n"))
        .expect_err("non-call contexts must not implicitly thunk values");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `() -> I32`, found `I32`")
    }));
}

#[test]
fn direct_trait_method_matches_beat_implicit_thunking() {
    let module = type_check(concat!(
        "trait Direct T { choose: T -> I32 }\n",
        "trait Lazy T { choose: (() -> T) -> I32 }\n",
        "impl Direct I32 { def choose = value => value }\n",
        "impl Lazy I32 { def choose = callback => callback () }\n",
        "let answer = choose 42\n",
    ));
    let answer = module.syntax().items.last().expect("answer binding");
    let Item::Binding(answer) = answer else {
        panic!("expected answer binding");
    };
    assert_eq!(
        module.type_of_expression(answer.value.as_ref().unwrap().syntax().id),
        Some(&CheckedType::I32),
    );
}

#[test]
fn trait_methods_fall_back_to_implicit_thunking() {
    let module = type_check(concat!(
        "trait Lazy T { force: (() -> T) -> T }\n",
        "impl Lazy I32 { def force = callback => callback () }\n",
        "let answer = force 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("trait dispatch should use thunking when no direct match exists");
}

#[test]
fn specializes_generic_effect_parameters() {
    let module = type_check(concat!(
        "use std.io.(IO, println)\n",
        "def twice: <effect E> (() ->{E} ()) ->{E} () = f => { f (); f () }\n",
        "def pure: () -> () = () => ()\n",
        "def output: () ->{IO} () = () => println \"hello\"\n",
        "let mut count = 0\n",
        "def update: () ->{state} () = () => { count = count + 1 }\n",
        "twice pure\ntwice output\ntwice update\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("effect-polymorphic calls should monomorphize");
}

#[test]
fn infers_open_effect_rows_and_checks_fixed_effects() {
    type_check(concat!(
        "use std.io.(IO, println)\n",
        "def run_and_log: <effect E> (() ->{E} ()) ->{E, IO} () = f => { f (); println \"done\" }\n",
        "def pure: () -> () = () => ()\n",
        "let mut count = 0\n",
        "def update: () ->{state} () = () => { count = count + 1 }\n",
        "run_and_log pure\nrun_and_log update\n",
    ));
}

#[test]
fn infers_checks_and_lowers_typed_resources() {
    let module = type_check(concat!(
        "type Clock = (now: () -> I32)\n",
        "type Logger = (write: I32 -> ())\n",
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
        .compile_module(&module)
        .expect("typed resources should lower to hidden parameters");
    assert!(llvm.contains("define i32 @read"));
}

#[test]
fn captured_mutable_cells_contribute_state_effects_and_metadata() {
    use stapler::CheckedStateEffect;

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
        .expect_err("state.write does not cover a captured-state read");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("requires effects {state.read}") })
    );
}

#[test]
fn passing_a_captured_cell_to_a_mutating_parameter_is_a_state_write() {
    use stapler::CheckedStateEffect;

    let module = type_check(concat!(
        "def replace: mut I32 -> () = value => { value = 2 }\n",
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
    use stapler::CheckedStateEffect;

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
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "type MoveOnly = CString\n",
            "def invalid: () ->{MoveOnly} () = () => ()\n",
        )))
        .expect_err("move-only resources must be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("concrete, sized, Copy nominal type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Clock = (now: () -> I32)\n",
            "def need = () => (resource Clock).now ()\n",
            "def invalid: () ->{} I32 = () => need ()\n",
        )))
        .expect_err("undeclared resources must be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("not contained in its declared effect set")
    }));
}

#[test]
fn standard_io_is_a_compiler_provided_resource_and_propagates_to_main() {
    let module = type_check(concat!(
        "use std.io.(IO, print, println)\n",
        "def identity: <T> T -> T = value => value\n",
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
        .compile_module(&module)
        .expect("generic calls should remain specialized during resource inference");
}

#[test]
fn allows_io_at_entry_module_top_level_but_rejects_non_builtin_opaque_resources() {
    // `IO` is implicitly available to the entry module's top-level
    // items, so a bare `println` there now compiles.
    type_check("use std.io.println\nprintln \"entry module output\"\n");

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "type Token = opaque\ndef use_token: () ->{Token} () = () => ()\n",
        ))
        .expect_err("only std.io.IO may be an opaque resource");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("concrete, sized, Copy nominal type")
    }));
}

#[test]
fn resources_obey_alias_exactness_macro_trait_and_boundary_rules() {
    let module = type_check(concat!(
        "type Clock = I32\n",
        "type alias CurrentClock = Clock\n",
        "type Logger = I32\n",
        "type Box T = (value: T)\n",
        "trait Observe T { observe: T ->{Clock} Clock }\n",
        "impl Observe I32 { def observe = value => resource CurrentClock }\n",
        "macro request = _: Ident \"clock\" => parse_quote { resource CurrentClock }\n",
        "def generated = () => request clock\n",
        "def declared: () ->{Logger, Clock, Clock} () = () => ()\n",
        "def through_trait = () => observe 1\n",
        "def boxed: () ->{Box I32} I32 = () => (resource Box I32).value\n",
        "with Clock = Clock 7 {\n",
        "  with Logger = Logger 8 { declared (); generated (); through_trait () }\n",
        "}\n",
        "with Box I32 = Box (value: 9) { boxed () }\n",
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
        .compile_module(&module)
        .expect("trait and specialized resource calls should lower");

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Clock = I32\n",
            "def need: () ->{Clock} Clock = () => resource Clock\n",
            "let incompatible: () -> Clock = need\n",
        )))
        .expect_err("resource-bearing and pure function types must compare exactly");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("expected `() -> Clock`")
            && diagnostic.message.contains("->{Clock}")
    }));

    for (source, expected) in [
        (
            "type Clock = I32\nresource Clock\n",
            "top-level initialization requires resources",
        ),
        (
            "type Clock = I32\nextern \"c\" { let read: () ->{Clock} Clock }\n",
            "external functions cannot require Staple resources",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("resource boundary must be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected {expected:?}, got {diagnostics:?}"
        );
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        construction_for("Ref"),
        Some(RecursiveConstruction::ManagedReference)
    );
    assert_eq!(
        construction_for("CallExpr"),
        Some(RecursiveConstruction::Syntax)
    );
    assert_eq!(construction_for("String"), None);
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after the Unix epoch")
        .as_nanos();
    let temporary = std::env::temp_dir().join(format!(
        "stapler-string-contract-{}-{nonce}",
        std::process::id()
    ));
    copy_directory(&root.join("stdlib"), &temporary);
    let production_string = include_str!("../stdlib/std/core/string.sta");
    let production_body = production_string
        .split_once("pub type String = Slice U8\n")
        .map(|(_, body)| body)
        .expect("production String module has its canonical declaration");
    std::fs::write(
        temporary.join("std/core/string.sta"),
        format!("{declaration}\n{production_body}"),
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
fn supports_repeated_spread_and_erased_product_references() {
    let source = concat!(
        "let explicit: I32[3] = (1, 2, 3)\n",
        "let spread: (String, ...I32[2]) = (\"x\", 4, 5)\n",
        "let fixed: Ref I32[3] = Ref explicit\n",
        "let erased: Slice I32 = fixed\n",
        "let constructed: Slice I32 = Ref (6, 7)\n",
        "let singleton: Slice I32 = Ref 8\n",
        "let empty: Slice I32 = Ref ()\n",
        "let count: USize = Slice.length erased\n",
        "let fixed_count: USize = Slice.length fixed\n",
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
        .compile_module(&module)
        .expect("product extensions should generate LLVM");
    assert!(llvm.contains("erased_ref.length"));
    assert!(llvm.contains("index.out_of_bounds"));
    assert!(llvm.contains("llvm.trap"));
}

#[test]
fn spreads_fixed_product_values_and_call_arguments() {
    let source = concat!(
        "def sum: I32[4] -> I32 = (a, b, c, d) => a + b + c + d\n",
        "let pair = (left: 2, right: 3)\n",
        "let arguments = (2, 3)\n",
        "let expanded = (prefix: \"value\", ...pair, suffix: False)\n",
        "let selected: I32 = expanded.left + expanded.right\n",
        "let answer: I32 = sum (1, ...arguments, 4)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("product value spreads should generate LLVM");
    assert!(llvm.contains("product.spread.element"));
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
        .compile_module(&module)
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
        .expect_err("a named spread requires a known expected product type");
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
        .expect_err("a required field left unfilled should be rejected");
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
        .expect_err("a field absent from the expected type should be rejected");
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
        .expect_err("an operand with unnamed elements cannot be named-spread");
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
        .expect_err("a positional spread cannot combine with a named spread");
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
        .compile_module(&module)
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
            "let value: (a: I32, b: I32) = (1, .a: 2, .b: 3)\n",
            "initialized more than once",
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
            "type alias Pair = (a: I32, b: I32)\nlet value: (...Pair, a: I32)\n",
            "duplicate product field name `a`",
        ),
    ];
    for (source, expected) in cases {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("invalid product should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected diagnostic containing `{expected}`, got {diagnostics:?}"
        );
    }
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
        .compile_module(&module)
        .expect("named spread overrides should remain valid");
}

#[test]
fn type_checks_and_lowers_mutable_places_and_ref_replace() {
    let source = concat!(
        "let mut value = 1\n",
        "value = 2\n",
        "let mut pair = (x: 3, y: 4)\n",
        "pair.x = value\n",
        "let mut fixed: Ref I32[2] = Ref (5, 6)\n",
        "fixed.0 = pair.x\n",
        "let index: USize = 1\n",
        "fixed[index] = 7\n",
        "let mut scalar = Ref 8\n",
        "let old = Ref.replace (scalar, 9)\n",
        "def local = () => { let mut inside = 10; inside = old; inside }\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("mutable places should generate LLVM");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("place.field"));
    assert!(llvm.contains("ref.replace.old"));
}

#[test]
fn type_checks_the_two_binding_mutability_forms() {
    // `let`: neither reassignable nor mutable.
    let diagnostics = TypeChecker::new()
        .check(resolve("let a = (x: 1, y: 2)\na = (x: 3, y: 4)\n"))
        .expect_err("`let` cannot be reassigned");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
    let diagnostics = TypeChecker::new()
        .check(resolve("let a = (x: 1, y: 2)\na.x = 3\n"))
        .expect_err("`let` cannot be written through");
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
        "type Box = I32\n",
        "def parameter = (value: I32) => { let mut value = value; value = value + 1; value }\n",
        "def field_write: mut Ref (x: I32, y: I32) -> () = value => { value.x = 1 }\n",
        "def matched = (value: Box) => match value { Box (mut inner) => { inner = 2; inner } }\n",
    ));

    type_check("def by_value: mut (x: I32, y: I32) -> () = value => { value.x = 1 }\n");

    // A `pub` global follows the same rules from another module as it does locally.
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Empty\n",
            "pub let a = (x: 1, y: 2)\n",
            "a.x = 3\n",
        )))
        .expect_err("`pub let` cannot be written through even in its own module");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn writes_through_a_ref_require_mut_on_a_named_root() {
    // A named `Ref`-typed binding needs `mut` to be written through.
    type_check(concat!(
        "let mut cell: Ref I32[2] = Ref (1, 2)\n",
        "cell.0 = 3\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(resolve("let cell: Ref I32[2] = Ref (1, 2)\ncell.0 = 3\n"))
        .expect_err("writing through a `Ref` requires `mut` on the binding");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );

    // A rootless `Ref` temporary remains writable regardless.
    type_check(concat!(
        "def make_ref = () => Ref (3, 4)\n",
        "(make_ref ()).0 = 5\n",
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
        .compile_module(&module)
        .expect("captured mut binding should generate LLVM");
    assert!(llvm.contains("binding.cell"));
}

fn function_mutations(module: &stapler::TypedModule, name: &str) -> Vec<CheckedMutation> {
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
        "def f = (mut p: Ref (I32, I32)) => { p.0 = 1 }\n",
        "def g = () => { let mut pair: Ref (I32, I32) = Ref (1, 2); f pair }\n",
    ));
    assert_eq!(
        function_mutations(&module, "f"),
        vec![CheckedMutation::Element(0)]
    );
}

#[test]
fn rejects_a_declared_empty_effect_set_when_the_body_mutates_a_parameter() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def f: Ref (I32, I32) -> () = p => { p.0 = 1 }\n"))
        .expect_err("an empty declared effect set forbids mutation");
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
            "def f: (mut a: Ref (I32, I32), b: Ref (I32, I32)) -> () = (a, b) => { b.0 = 1 }\n",
        )))
        .expect_err("writing `b` exceeds the declared `mut a`");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn resolves_named_and_positional_mutation_targets_to_the_same_type() {
    let by_index =
        type_check("def f: (mut a: Ref (I32, I32), b: I32) -> () = (a, b) => { a.0 = 1 }\n");
    let by_name =
        type_check("def f: (mut a: Ref (I32, I32), b: I32) -> () = (a, b) => { a.0 = 1 }\n");
    assert_eq!(
        function_mutations(&by_index, "f"),
        function_mutations(&by_name, "f"),
    );
}

#[test]
fn a_whole_parameter_declaration_covers_writing_a_single_element() {
    type_check("def f: mut Ref (I32, I32) -> () = p => { p.0 = 1 }\n");
}

#[test]
fn call_site_mutation_requires_an_explicit_caller_marker() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def f: mut Ref (I32, I32) -> () = p => { p.0 = 1 }\n",
            "def g = (q: Ref (I32, I32)) => { f q }\n",
        )))
        .expect_err("mutation permissions must not propagate into callers");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );

    let module = type_check(concat!(
        "def f: mut Ref (I32, I32) -> () = p => { p.0 = 1 }\n",
        "def g = (mut q: Ref (I32, I32)) => { f q }\n",
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
            "def f: mut Ref (I32, I32) -> () = p => { p.0 = 1 }\n",
            "def g = () => { let pair: Ref (I32, I32) = Ref (1, 2); f pair }\n",
        )))
        .expect_err("the caller's binding must be declared `mut`");
    assert!(diagnostics.iter().any(|d| {
        d.message
            .contains("cannot write through `pair`; its binding is not declared `mut`")
    }));
}

#[test]
fn a_closure_may_mutate_an_explicitly_marked_captured_parameter() {
    let module = type_check(concat!(
        "def outer = (mut p: Ref (I32, I32)) => {\n",
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
            "trait Reset T { reset: T -> () }\n",
            "impl Reset (Ref (I32, I32)) { def reset = p => { p.0 = 0 } }\n",
        )))
        .expect_err("the impl mutates a parameter its trait does not declare");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("not declared `mut`"))
    );
}

#[test]
fn an_explicit_mut_effect_passes_through_a_ref_crossing_local_alias() {
    let module = type_check(concat!(
        "type MyInt = Ref I32\n",
        "def mutate_my_int: (mut MyInt, I32) -> () = (my_int, value) => {\n",
        "  let MyInt mut inner = my_int\n",
        "  Ref.replace (inner, value)\n",
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
            "type MyInt = Ref I32\n",
            "def mutate_my_int: (mut MyInt, I32) -> () = (my_int, value) => {\n",
            "  let MyInt mut inner = my_int\n",
            "  Ref.replace (inner, value)\n",
            "  ()\n",
            "}\n",
            "def bad = () => {\n",
            "  let my_int: MyInt = MyInt (Ref 1)\n",
            "  mutate_my_int (my_int, 42)\n",
            "}\n",
        )))
        .expect_err("the caller's `my_int` must be declared `mut` too");
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
        "type Clock = I32\n",
        "def f: mut Ref (I32, I32) -> () = p => { p.0 = 1 }\n",
        "def g: mut Ref (I32, I32) ->{Clock} () = p => { p.0 = 1 }\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("mut effects should not affect the ABI");
    let base = llvm_parameter_count(&llvm, "f");
    let with_resource = llvm_parameter_count(&llvm, "g");
    // Adding the `Clock` resource on top of the same `mut` effect adds
    // exactly its own one hidden parameter, not a second one for `mut`
    // (which contributes nothing to the ABI either way).
    assert_eq!(with_resource, base + 1);
}

#[test]
fn a_mutated_ref_parameter_lowers_and_the_caller_observes_the_write() {
    let module = type_check(concat!(
        "def f: mut Ref (I32, I32) -> () = p => { p.0 = p.0 + 1 }\n",
        "def use_it = () => { let mut pair: Ref (I32, I32) = Ref (1, 2); f pair; pair.0 }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a mutated Ref parameter should lower");
}

#[test]
fn a_mut_parameter_can_replace_the_callers_value() {
    let module = type_check(concat!(
        "def update_data: mut I32 -> () = data => { data = 42 }\n",
        "let mut data: I32 = 1\n",
        "update_data data\n",
    ));
    assert_eq!(
        function_mutations(&module, "update_data"),
        vec![CheckedMutation::Whole]
    );
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
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
        .expect_err("an unmarked parameter must not infer mutation");
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
        "impl Replace I32 { def replace = mut value => { value = 2 } }\n",
    ));

    for source in [
        "def mismatch: (I32, mut I32) -> () = (mut first, second) => ()\n",
        concat!(
            "trait Replace T { replace: (mut T, T) -> () }\n",
            "impl Replace I32 { def replace = (first, mut second) => () }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("parameter markers and declared mutation permissions must match");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("parameter `mut` markers declare")
        }));
    }
}

#[test]
fn resource_inference_preserves_parameter_declared_mutation() {
    let module = type_check(concat!(
        "type Clock = I32\n",
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
        "def replace_pair: mut (I32, I32) -> () = pair => { pair = (3, 4) }\n",
        "def replace_first: (mut I32, I32) -> () = (first, _) => { first = 9 }\n",
        "let mut pair = (1, 2)\n",
        "replace_pair pair\n",
        "let mut first = 0\n",
        "replace_first (first, 8)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("whole and positional product replacements should lower");
}

#[test]
fn a_first_class_mutating_function_preserves_writeback() {
    let module = type_check(concat!(
        "def replace: mut I32 -> () = value => { value = 7 }\n",
        "let operation: mut I32 -> () = replace\n",
        "let mut value = 1\n",
        "operation value\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("indirect mutating calls should use the address-passing ABI");
}

#[test]
fn a_mut_call_materializes_and_discards_a_temporary_argument() {
    let module = type_check(concat!(
        "def update_data: mut I32 -> () = data => { data = 42 }\n",
        "update_data (1 + 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a temporary mut argument should get call-scoped storage");
    assert!(llvm.contains("mutation.temporary"));
}

#[test]
fn a_move_only_mut_temporary_drops_replaced_and_final_values() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "def replace: mut CString -> () = value => { value = c_string \"replacement\" }\n",
        "replace (c_string \"initial\")\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("move-only mutation temporaries should lower with ownership cleanup");
    assert!(llvm.contains("mutation.temporary.final"));
    assert!(llvm.contains("assignment.old"));
}

#[test]
fn rejects_invalid_assignment_targets_and_uninitialized_mutable_lets() {
    let diagnostics = NameResolver::new()
        .resolve(&parse("let mut value: I32\n").expect("syntax should parse"))
        .expect_err("mutable lets require an initializer");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("require an initializer"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("let value = 1\nvalue = 2\n"))
        .expect_err("immutable names cannot be reassigned");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("not declared `mut`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def product = () => (x: 1, y: 2)\nproduct ().x = 3\n",
        ))
        .expect_err("by-value temporaries are not writable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("writable place"))
    );
}

#[test]
fn lowers_move_only_mutation_reinitialization_and_captured_cells() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "def local = (first: CString, second: CString) => {\n",
        "  let mut value = first\n",
        "  value = second\n",
        "  let moved = value\n",
        "  value = c_string \"replacement\"\n",
        "  drop moved\n",
        "  drop value\n",
        "}\n",
        "def captured = (initial: CString) => {\n",
        "  let mut value = initial\n",
        "  () => { let old = value; value = c_string \"next\"; drop old }\n",
        "}\n",
        "def managed = (initial: CString, next: CString) => {\n",
        "  let mut reference = Ref initial\n",
        "  let old = Ref.replace (reference, next)\n",
        "  drop old\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("move-only mutable cells should generate LLVM");
    assert!(llvm.contains("cell.drop.is_live"));
    assert!(llvm.contains("__staple_gc_finalize_cell_"));
    assert!(llvm.contains("ref.replace.old"));
}

#[test]
fn supports_mutable_parameter_match_and_copy_ref_pattern_binders() {
    type_check(concat!(
        "type Box = I32\n",
        "type Empty\n",
        "def parameter = (value: I32) => { let mut value = value; value = value + 1; value }\n",
        "def matched = (value: Box | Empty) => match value { Box (mut inner) => { inner = 3; inner }, Empty() => 0 }\n",
        "def borrowed = (value: Ref I32) => { let Ref (mut inner) = value; inner = 4; inner }\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Resource = I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "def invalid = (value: Ref Resource) => { let Ref (mut inner) = value; inner }\n",
        )))
        .expect_err("move-only Ref borrows cannot become mutable locals");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("borrowed through `Ref` cannot be bound as `mut`")
    }));
}

#[test]
fn rejects_invalid_product_spreads_and_indices() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: (...I32)\n"))
        .expect_err("a scalar cannot be spread");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot spread non-product type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid = (...1, 2)\n"))
        .expect_err("a scalar value cannot be spread");
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
        .expect_err("a product containing a move-only element cannot derive Index");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("no `Index` implementation") })
    );
}

#[test]
fn derives_trait_delegated_product_indexing() {
    let source = concat!(
        "def select: <A, B where Copy A, Copy B> ((A, B), USize) -> A | B = (pair, position) => pair[position]\n",
        "let pair = (1, \"two\")\n",
        "let position: USize = 1\n",
        "let generic_selected: I32 | String = select (pair, position)\n",
        "let collapsed = select ((1, 2), position)\n",
        "let selected: I32 | String = pair[position]\n",
        "let operation: ((I32, String), USize) -> I32 | String = Index.index\n",
        "let selected_again: I32 | String = operation (pair, position)\n",
        "let mut updated: I32[3] = (1, 2, 3)\n",
        "updated[position] = 9\n",
        "let mutate_by_value: (mut I32[3], USize, I32) -> () = MutateIndex.mutate_index\n",
        "mutate_by_value (updated, position, 10)\n",
        "let mut fixed: Ref I32[3] = Ref updated\n",
        "let mutate_operation: (mut Ref I32[3], USize, I32) -> () = MutateIndex.mutate_index\n",
        "mutate_operation (fixed, position, 6)\n",
        "fixed[position] = 7\n",
        "let mut erased: Slice I32 = fixed\n",
        "erased[position] = 8\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("derived indexing traits should generate LLVM");
    assert!(llvm.contains("structural_Index"));
    assert!(llvm.contains("structural_MutateIndex"));
    assert!(llvm.contains("index.case"));
}

#[test]
fn delegates_brackets_to_explicit_indexing_implementations() {
    let source = concat!(
        "impl Index I32 String I32 { def index = (target, position) => target }\n",
        "impl MutateIndex I32 String I32 { def mutate_index = (target, position, value) => () }\n",
        "let selected: I32 = 4[\"key\"]\n",
        "4[\"key\"] = 5\n",
    );
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("custom indexing traits should generate LLVM");
}

#[test]
fn allows_by_value_indexed_assignment_for_mut_bindings() {
    let source = "let mut values: I32[2] = (1, 2)\nlet position: USize = 0\nvalues[position] = 3\n";
    let module = type_check(source);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("by-value indexed assignment through a `mut` binding should generate LLVM");
}

#[test]
fn rejects_by_value_indexed_assignment_without_mut_binding() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let values: I32[2] = (1, 2)\nlet position: USize = 0\nvalues[position] = 3\n",
        ))
        .expect_err("by-value product assignment requires a `mut` binding");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("not declared `mut`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let values: Ref I32[2] = Ref (1, 2)\nvalues[2] = 3\n",
        ))
        .expect_err("known out-of-bounds MutateIndex must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("out of bounds"))
    );
}

#[test]
fn derives_mutate_index_for_move_only_homogeneous_products() {
    let source = concat!(
        "use std.cinterop.CString\n",
        "def mutate_by_value = (mut values: CString[2], position: USize, replacement: CString) => { ",
        "values[position] = replacement; () }\n",
        "def mutate = (mut values: Ref CString[2], position: USize, replacement: CString) => { ",
        "values[position] = replacement; () }\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("move-only indexed replacement should generate LLVM");
    assert!(llvm.contains("index.old"));
}

#[test]
fn rejects_overlapping_structural_indexing_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "impl Index I32[2] USize I32 {\n",
            "  def index = (values, position) => values.0\n",
            "}\n",
        )))
        .expect_err("structural product Index cannot be overridden");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("derived structurally for this product type")
    }));
}

#[test]
fn derives_iterator_item_as_a_sum_of_product_elements() {
    type_check(concat!(
        "let iterator: (I32[3], USize) = IntoIterator.into_iterator (1, 2, 3)\n",
        "let homogeneous_step: IterStep ((I32[3], USize), I32) = Iterator.next iterator\n",
        "let mixed: (I32, String, I32) = (1, \"two\", 3)\n",
        "let mixed_iterator: ((I32, String, I32), USize) = IntoIterator.into_iterator mixed\n",
        "let mixed_step: IterStep (((I32, String, I32), USize), I32 | String) = Iterator.next mixed_iterator\n",
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
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("derived iteration traits should generate LLVM");
    assert!(llvm.contains("structural_IntoIterator"));
    assert!(llvm.contains("structural_Iterator"));
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
        .expect_err("a product containing a move-only element cannot derive IntoIterator");
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
            "impl IntoIterator I32[2] (I32[2], USize) {\n",
            "  def into_iterator = value => (value, 0 satisfies USize)\n",
            "}\n",
        )))
        .expect_err("structural product IntoIterator cannot be overridden");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("derived structurally for this product type")
    }));
}

#[test]
fn rejects_erased_products_outside_refs_and_ref_destructuring() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: I32[]\n"))
        .expect_err("an erased product cannot be used by value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("unsized type") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let fixed: Ref I32[2] = Ref (1, 2)\nlet erased: Slice I32 = fixed\nlet Ref values = erased\n",
        ))
        .expect_err("an erased reference cannot be destructured");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("cannot be destructured") })
    );
}

#[test]
fn handles_product_repetition_edges_and_limits() {
    type_check("let value: (...I32[0], ...I32[1], I32) = (1, 2)\n");

    let diagnostics = TypeChecker::new()
        .check(resolve("let too_large: I32[65536]\n"))
        .expect_err("oversized repeated products must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("limit of 65535") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let pair: I32[2] = (1, 2)\nlet invalid = pair[2]\n",
        ))
        .expect_err("known out-of-bounds indices must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("out of bounds") })
    );
}

#[test]
fn aliases_complete_erased_references_and_unsized_types_but_rejects_ffi() {
    type_check(concat!(
        "type alias Ints = Slice I32\n",
        "let fixed: Ref I32[2] = Ref (1, 2)\n",
        "let values: Ints = fixed\n",
        "let count: USize = Slice.length values\n",
    ));

    type_check("type alias MySlice = I32[]\n");

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type alias MySlice = I32[]\n",
            "let fixed: Ref I32[2] = Ref (1, 2)\n",
            "let values: Ref MySlice = fixed\n",
        )))
        .expect_err("aliasing an unsized array does not let it bypass `Slice`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("use `Slice T` instead") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "type alias MySlice = I32[]\nlet invalid: MySlice\n",
        ))
        .expect_err("unsized aliases cannot be used by value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("unsized type") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("extern \"c\" { let invalid: Ref I32[] -> I32 }\n"))
        .expect_err("`Ref I32[]` is rejected before an FFI-specific check even runs");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("use `Slice T` instead") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("extern \"c\" { let invalid: Slice I32 -> I32 }\n"))
        .expect_err("erased references must not cross the FFI");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("external binding types cannot contain erased products")
    }));
}

#[test]
fn enforces_implicit_sized_and_supports_question_sized_parameters() {
    type_check(concat!(
        "def preserve: <T where ?Sized T> Ref T -> Ref T = value => value\n",
        "def explicitly_sized: <T where ?Sized T, Sized T> Ref T -> Ref T = value => value\n",
        "let fixed: Ref I32[2] = Ref (1, 2)\n",
        "let erased: Slice I32 = fixed\n",
        "let same: Slice I32 = preserve erased\n",
        "let same_fixed: Ref I32[2] = explicitly_sized fixed\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def sized_only: <T> Ref T -> Ref T = value => value\n",
            "let fixed: Ref I32[2] = Ref (1, 2)\n",
            "let erased: Slice I32 = fixed\n",
            "let invalid = sized_only erased\n",
        )))
        .expect_err("ordinary generic parameters have an implicit Sized bound");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("implicit `Sized` bound") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def invalid: <T where ?Sized T> T -> () = value => ()\n",
        ))
        .expect_err("a relaxed parameter cannot be passed by value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("must be sized") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("impl Sized I32 {}\n"))
        .expect_err("Sized is structural");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`Sized` is implemented structurally")
    }));
}

#[test]
fn derives_default_structurally_for_products() {
    let source = concat!(
        "type Seed = I32\n",
        "impl Default Seed { def default = () => Seed 7 }\n",
        "let integers: I32[3] = default ()\n",
        "let mixed: (I32, Bool, String) = default ()\n",
        "let nested: I32[2][2] = default ()\n",
        "let seeds: Seed[2] = default ()\n",
        "let answer: I32 = integers.0 + integers.1 + integers.2\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("derived product defaults should generate LLVM");
    assert!(llvm.contains("default.element"));
    assert!(llvm.contains("default.call"));
}

#[test]
fn rejects_default_for_products_with_non_default_elements() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: (Ref I32)[2] = default ()\n"))
        .expect_err("Ref has no Default implementation");
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
        .expect_err("unknown name should not resolve");

    assert_eq!(
        diagnostics[0].to_string(),
        "unknown name `missing` at line 2, column 1"
    );
}

#[test]
fn infers_and_checks_function_return_types() {
    let module = type_check(concat!(
        "let first = (a: I32, b: I32) => a\n",
        "let second = (a: I32, b: I32) => b satisfies I32\n",
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
fn checks_general_satisfies_expressions_and_contextually_types_functions() {
    type_check(concat!(
        "let small = 42 satisfies I8\n",
        "let identity = (value => value) satisfies I32 -> I32\n",
        "let result: I32 = identity 42\n",
    ));

    let module = resolve("let invalid = \"text\" satisfies I32\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("an expression must satisfy its asserted type");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `I32`, found `String`")
    }));
}

#[test]
fn infers_and_lowers_nominal_sums_with_propagation() {
    let module = type_check(concat!(
        "pub(repr) type Ok T = T\n",
        "pub(repr) type IOError = String\n",
        "def read: String -> Ok String | IOError = path => Ok(path)\n",
        "def parse = (path: String) => { let Ok(file)? = read(path); Ok(file) }\n",
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
        .compile_module(&module)
        .expect("sum propagation should generate LLVM");
    assert!(llvm.contains("propagate.ok"));
    assert!(llvm.contains("propagate.return"));
}

#[test]
fn exhaustively_matches_sum_values_and_destructures_payloads() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "def choose = result: Ok I32 | IOError => match result {\n",
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
        .compile_module(&module)
        .expect("match should generate LLVM");
    assert!(llvm.contains("match.tag"));
    assert!(llvm.contains("match.tag.matches"));
    assert!(llvm.contains("match.value"));
}

#[test]
fn matches_singletons_and_joins_nominal_arm_results() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
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
        .compile_module(&module)
        .expect("singleton match should generate LLVM");
}

#[test]
fn supports_match_catch_alls_wildcard_parameters_and_returning_arms() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "def preserve: (Ok I32 | IOError) -> Ok I32 | IOError = result => match result {\n",
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
        .compile_module(&module)
        .expect("catch-all and returning matches should generate LLVM");
}

#[test]
fn matches_values_of_any_runtime_type() {
    let module = type_check(concat!(
        "pub(repr) type Wrapped = I32\n",
        "def integer = value: I32 => match value { number => number }\n",
        "def float = value: F64 => match value { _: F64 => value }\n",
        "def identity = value: I32 => value\n",
        "def function = value: (I32 -> I32) => match value { callable => callable 4 }\n",
        "def nominal = value: Wrapped => match value { Wrapped number => number }\n",
        "def generic: <T> T -> T = value => match value { same: T => same }\n",
        "integer 1\n",
        "float 1.5\n",
        "function identity\n",
        "nominal (Wrapped 2)\n",
        "generic 3\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        .expect_err("an incompatible typed pattern should fail");
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
        .expect_err("an arm after a scalar catch-all should be unreachable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unreachable match arm"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def invalid = result: Ok I32 | IOError => match result { Ok value => value, }\n",
            "invalid (Ok 1)\n",
        )))
        .expect_err("non-exhaustive matches should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("non-exhaustive match")
            && diagnostic.message.contains("IOError")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def invalid = result: Ok I32 | IOError => match result {\n",
            "  Ok value => value,\n",
            "  Ok other => other,\n",
            "  _ => 0,\n",
            "  IOError _ => 1,\n",
            "}\n",
            "invalid (Ok 1)\n",
        )))
        .expect_err("unreachable match arms should fail");
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
        .compile_module(&module)
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
        .expect_err("incomplete product match should fail");
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
        .expect_err("duplicate product arm should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unreachable match arm"))
    );
}

#[test]
fn injects_and_widens_sum_values() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "pub(repr) type ParseError = String\n",
        "type alias Result T = Ok T | IOError\n",
        "def small: () -> Ok I32 | IOError = () => Ok(1)\n",
        "let reordered: IOError | Ok I32 = small()\n",
        "let aliased: Result I32 = reordered\n",
        "let duplicate: Ok I32 | Ok I32 = Ok(3)\n",
        "let pair: (Result I32, I32) = (aliased, 2)\n",
        "def captured = () => { let local: Result I32 = pair.0; let inner = () => local; inner() }\n",
        "def wide: () -> Ok I32 | IOError | ParseError = () => small()\n",
        "captured()\n",
        "wide()\n",
    ));
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    generator
        .compile_module(&module)
        .expect("sum injection and widening should generate valid LLVM");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow the Unix epoch")
        .as_nanos();
    let output = std::env::temp_dir().join(format!("stapler-sum-test-{nonce}.o"));
    generator
        .emit_object(&module, &output, None)
        .expect("sum values should emit a native object");
    std::fs::remove_file(output).expect("temporary sum object should be removable");
}

#[test]
fn propagates_each_residual_variant_and_joins_explicit_returns() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "pub(repr) type ParseError = String\n",
        "def fail: () -> Ok I32 | IOError = () => IOError(\"io\")\n",
        "def parse = () => { let Ok(value)? = fail(); return ParseError(\"parse\"); }\n",
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
        .compile_module(&module)
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
        "let applied: Ok I32 | Ok String = Ok(3)\n",
        "def small: () -> I32 | String = () => 7\n",
        "let widened: I32 | String | F64 = small()\n",
        "def generic: <T> T -> T | String = value => value\n",
        "generic 9\n",
        "select(integer)\n",
        "select(text)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("arbitrary sized sum alternatives should lower to LLVM");
}

#[test]
fn rejects_unsized_sum_alternatives_and_ambiguous_nominal_patterns() {
    for (source, expected) in [
        ("let value: I32[] | String\n", "must be a sized type"),
        (
            concat!(
                "def inspect = value: Ok I32 | Ok String => match value {\n",
                "  Ok(payload) => 0,\n",
                "  _ => 1,\n",
                "}\n",
            ),
            "selects more than one alternative",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("invalid sum should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn rejects_invalid_propagation_and_sum_ffi() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def invalid = () => { let Ok(value)? = Ok(1); Ok(value) }\n",
        )))
        .expect_err("propagation requires a sum");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("propagating binding requires a sum value")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def read: () -> Ok I32 | IOError = () => Ok(1)\n",
            "def invalid: () -> Ok I32 = () => { let Ok(value)? = read(); Ok(value) }\n",
        )))
        .expect_err("explicit result should contain propagated variants");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("propagated variant `IOError` is not contained")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "extern \"c\" { let invalid: () -> Ok I32 | IOError }\n",
        )))
        .expect_err("sum ABI should remain internal");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("external binding types cannot contain sums")
    }));
}

#[test]
fn rejects_propagation_outside_functions_and_non_nominal_roots() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "pub(repr) type IOError = String\n",
                "let Ok(value)? = Ok(1)\n",
                "def invalid = () => { let (Ok(value), other)? = (Ok(1), 2); Ok(value) }\n",
            ),
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("invalid propagation contexts should not resolve");
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
        .expect_err("an immediate forward read must fail");
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
        .compile_module(&module)
        .expect("capturing a forward binding must remain legal");
    assert!(llvm.contains("_g_state"));
    assert!(llvm.contains("binding.uninitialized"));
    assert!(!llvm.contains("_f_state"));
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
        .compile_module(&module)
        .expect("unsafe local captures should share a binding cell");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("binding.uninitialized"));
}

#[test]
fn applies_initialization_state_to_recursive_local_generics() {
    let module = type_check(concat!(
        "def outer: () -> I32 = () => {\n",
        "  def recur: <T> T -> T = value => recur value\n",
        "  recur 1\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic functions use the same initialization state model");
    assert!(llvm.contains("binding.state.cell"));
    assert!(llvm.contains("binding.uninitialized"));
}

#[test]
fn does_not_add_state_metadata_to_safe_bindings() {
    let module = type_check("def answer = 42\nanswer\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("safe bindings should compile without state metadata");
    assert!(!llvm.contains("_answer_state"));
}

#[test]
fn rejects_an_incorrect_function_result_type() {
    let module = resolve("use std.cinterop.*\nlet answer = () => 42 satisfies CString\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("incorrect return type should fail");

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
        .expect_err("incorrect argument type should fail");

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
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("nested product pattern should compile");

    assert!(llvm.contains("define i32 @add_nested(ptr %0, i32 %x, <{ i32, i32 }>"));
    assert!(llvm.contains("extractvalue <{ i32, i32 }>"));
}

#[test]
fn binds_a_product_without_destructuring_it() {
    let module = type_check(concat!(
        "let sum = pair: (I32, I32) => pair.0\n",
        "sum (1, 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a product binding pattern should compile");

    assert!(llvm.contains("define i32 @sum(ptr %0, i32 %1, i32 %2)"));
    assert!(llvm.contains("extractvalue <{ i32, i32 }>"));
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
        .compile_module(&module)
        .expect("at-patterns over Copy products should compile");

    assert!(llvm.contains("define i32 @parameter(ptr %0, i32 %1, i32 %2)"));
    assert!(!llvm.contains("define i32 @parameter(ptr %0, <{ i32, i32 }>"));
    assert!(llvm.contains("%args"));
}

#[test]
fn at_patterns_are_structural_in_matches_and_propagation() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "def read: () -> Ok I32 | IOError = () => Ok(42)\n",
        "def choose = pair: (Bool, I32) => match pair {\n",
        "  whole@(True(), value) => whole.1 + value,\n",
        "  _ => 0,\n",
        "}\n",
        "def parse = () => { let result@Ok(value)? = read(); result }\n",
        "choose (True, 1)\n",
        "parse()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("at-patterns should preserve match and propagation lowering");
    assert!(llvm.contains("match.tag"));
    assert!(llvm.contains("propagate.ok"));
}

#[test]
fn at_patterns_require_copy_runtime_values_and_honor_copy_bounds() {
    type_check("def retain: <T where Copy T> T -> T = value@_ => value\n");

    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid: <T> T -> T = value@_ => value\n"))
        .expect_err("an unconstrained generic at-pattern should not be Copy");
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
        .expect_err("a move-only concrete at-pattern should be rejected");
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
        .compile_module(&module)
        .expect("compile-time at-patterns should clone syntax values");
}

#[test]
fn type_checks_transparent_aliases() {
    type_check("type alias number = I32\nlet answer: number = 42\n");
}

#[test]
fn uses_regular_prelude_functions_for_i32_arithmetic() {
    let module = type_check(concat!(
        "let sum = 1 + 2\n",
        "let difference = 4 - 3\n",
        "let product = 2 * 3\n",
        "let quotient = 8 / 2\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("prelude arithmetic should compile");

    assert!(llvm.contains("add i32"));
    assert!(llvm.contains("sub i32"));
    assert!(llvm.contains("mul i32"));
    assert!(llvm.contains("sdiv i32"));
    assert!(!llvm.contains("declare i32 @__i32_add"));
}

#[test]
fn compares_all_standard_library_integer_types() {
    let module = type_check(concat!(
        "let equal: Bool = 1 == 1\n",
        "let not_equal: Bool = 1 != 2\n",
        "let less: Bool = 1 < 2\n",
        "let less_equal: Bool = 1 <= 2\n",
        "let greater: Bool = 2 > 1\n",
        "let greater_equal: Bool = 2 >= 1\n",
        "def same: <T where Copy T, Eq T> T -> T -> Bool = left => right => left == right\n",
        "def before: <T where PartialOrd T> T -> T -> Bool = left => right => left < right\n",
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
        .compile_module(&module)
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
        "extern \"c\" { let scale_float: F64 -> F64 }\n",
        "def single = () => { let a: F32 = 1.5; let b: F32 = .5; (a + b) * b - a / b; } satisfies F32\n",
        "def double = () => { let a: F64 = 1e3; let b: F64 = 2.; (a + b) / b; } satisfies F64\n",
        "def defaulted = () => 1.25\n",
        "let less: Bool = 1.0 < 2.0\n",
        "let equal: Bool = 2.0 == 2.0\n",
        "let scaled: F64 = scale_float 2.0\n",
        "let unordered: Option Ordering = PartialOrd.partial_cmp (0.0 / 0.0) 1.0\n",
        "let ordered_less: Ordering = Ord.cmp 1 2\n",
        "let ordered_equal: Ordering = Ord.cmp 2 2\n",
        "let ordered_greater: Ordering = Ord.cmp 3 2\n",
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
        .compile_module(&module)
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
        .expect_err("overflowing F32 literal should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not fit in `F32`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("let value: F32 = 1\n"))
        .expect_err("integer literals should not become floats");
    assert!(!diagnostics.is_empty());

    let diagnostics = TypeChecker::new()
        .check(resolve("let value = 1e+\n"))
        .expect_err("an incomplete exponent should fail type checking");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("float literal"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("def compare: <T where Ord T> T -> T -> Ordering = left => right => Ord.cmp left right\nlet invalid = compare 1.0 2.0\n"))
        .expect_err("floats should not implement Ord");
    assert!(!diagnostics.is_empty());
}

#[test]
fn requires_only_the_core_ordering_methods() {
    let diagnostics = TypeChecker::new()
        .check(resolve("impl PartialOrd String {}\n"))
        .expect_err("partial_cmp is required");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing member `partial_cmp`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("impl Ord String {}\n"))
        .expect_err("cmp is required");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing member `cmp`"))
    );
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
        .compile_module(&module)
        .expect("Bool equality should dispatch through its standard-library implementation");
    assert!(llvm.contains("match.tag.matches"));
}

#[test]
fn lowercase_i32_is_an_ordinary_unresolved_type_name() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let value: i32 = 1\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("lowercase i32 should not name the builtin");

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
        "def require_true = (value: Bool) => { let True()? = value; True }\n",
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
        .compile_module(&module)
        .expect("library-defined Bool should use ordinary sum lowering");
}

#[test]
fn lowercase_bool_is_an_ordinary_unresolved_type_name() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let predicate: bool\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("lowercase bool should not name the builtin");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `bool`")
    );
}

#[test]
fn cinterop_types_require_an_explicit_import() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let text: CString\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("CString should not be in the prelude");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `CString`")
    );

    type_check(concat!(
        "use std.cinterop.*\n",
        "let character: CChar\n",
        "let pointer: CPointer CChar\n",
        "let text: CString\n",
    ));
}

#[test]
fn syntax_types_and_quote_require_an_explicit_import() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("type alias Captured = SyntaxNode\n", root)
        .expect("source should load without importing syntax names");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("syntax types should not be in the core prelude");
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
        .expect_err("quote should require an explicit import");
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
        "use std.cinterop.*\n",
        "extern \"c\" { let consume: (CPointer I32) -> I32 }\n",
        "consume (c_string \"wrong pointee\")\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("CString should only coerce to CPointer CChar");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `CPointer I32`, found `CString`")
    }));
}

#[test]
fn generic_opaque_arguments_are_part_of_type_identity() {
    let module = resolve(concat!(
        "type Handle T = opaque\n",
        "def invalid: Handle I32 -> Handle String = value => value\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("opaque applications with different arguments must differ");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `Handle String`, found `Handle I32`")
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
        "let mut values: Buffer I32 = Buffer.with_capacity (2 satisfies USize)\n",
        "let empty_length: USize = Buffer.length values\n",
        "let capacity: USize = Buffer.capacity values\n",
        "Buffer.push values 10\n",
        "Buffer.push values 20\n",
        "let first: Ref I32 = Buffer.get_ref values (0 satisfies USize)\n",
        "let popped: Option I32 = Buffer.pop values\n",
        "let frozen: Slice I32 = Buffer.freeze values\n",
        "let frozen_length: USize = Slice.length frozen\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Buffer operations should compile");
    assert!(llvm.contains("buffer.allocate"));
    assert!(llvm.contains("buffer.push.slot"));
    assert!(llvm.contains("buffer.pop.option"));
    assert!(llvm.contains("buffer.slice.pointer"));

    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "let mut owned: Buffer CString = Buffer.with_capacity (1 satisfies USize)\n",
        "Buffer.push owned (c_string \"owned\")\n",
    ));
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Buffer should own non-Default, non-Copy elements");
    assert!(llvm.contains("__staple_gc_finalize_buffer_"));
}

#[test]
fn buffer_transfer_type_checks_and_compiles() {
    let module = type_check(concat!(
        "let mut source: Buffer I32 = Buffer.with_capacity (2 satisfies USize)\n",
        "Buffer.push source 1\n",
        "Buffer.push source 2\n",
        "let mut destination: Buffer I32 = Buffer.with_capacity (5 satisfies USize)\n",
        "Buffer.push destination 0\n",
        "Buffer.transfer (source, destination)\n",
        "let moved_length: USize = Buffer.length destination\n",
        "let emptied_length: USize = Buffer.length source\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Buffer.transfer should compile");
    assert!(llvm.contains("buffer.transfer.aliased"));
    assert!(llvm.contains("buffer.transfer.insufficient_capacity"));
    assert!(llvm.contains("buffer.transfer.dest.write"));
    assert!(llvm.contains("llvm.memcpy"));
}

#[test]
fn list_grows_past_initial_capacity_and_type_checks() {
    let module = type_check(concat!(
        "let mut values: List I32 = List.new ()\n",
        "List.push values 1\n",
        "List.push values 2\n",
        "List.push values 3\n",
        "List.push values 4\n",
        "List.push values 5\n",
        "let length: USize = List.length values\n",
        "let capacity: USize = List.capacity values\n",
        "let first: Option I32 = List.get values (0 satisfies USize)\n",
        "let last: I32 = List.get_unchecked values (4 satisfies USize)\n",
        "let last_ref: Option (Ref I32) = List.get_ref values (4 satisfies USize)\n",
        "let last_ref_unchecked: Ref I32 = List.get_ref_unchecked values (4 satisfies USize)\n",
        "let popped: Option I32 = List.pop values\n",
        "let sized: List I32 = List.with_capacity (10 satisfies USize)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("List operations should compile");
    assert!(llvm.contains("buffer.transfer.dest.write"));
}

#[test]
fn list_get_requires_copy_element_type() {
    let resolved = resolve(concat!(
        "use std.cinterop.*\n",
        "let mut values: List CString = List.new ()\n",
        "List.push values (c_string \"a\")\n",
        "let first: CString = List.get values (0 satisfies USize)\n",
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
        "pub(repr) type Box T = (value: T)\n",
        "companion<T where Copy T> Box T {\n",
        "    pub def first: Box T -> T = Box value => value\n",
        "    pub def second: Box T -> T = Box value => value\n",
        "}\n",
        "let boxed: Box I32 = Box (value: 5)\n",
        "let a: I32 = Box.first boxed\n",
        "let b: I32 = Box.second boxed\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        "pub(repr) type Box T = (value: T)\n",
        "companion<T> Box T {\n",
        "    pub macro of = value: Expr => parse_quote { Box (value: $value) }\n",
        "}\n",
        "let boxed: Box I32 = Box.of 5\n",
        "let value: I32 = boxed.value\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a macro declared inside a companion should resolve as Type.macro");
}

#[test]
fn dispatches_generic_implementations_of_multi_parameter_functional_dependency_traits() {
    // Regression test: a generic `impl<T where Bound T> Trait Source Target`
    // of a multi-parameter trait with a functional dependency used to fail
    // dispatch entirely, even at a fully concrete call site, because the
    // dependency-completion and codegen dispatch paths compared a candidate
    // impl's (possibly still-generic) header against the query with plain
    // equality instead of unification.
    let module = type_check(concat!(
        "trait Bound T { check: T -> Bool }\n",
        "trait Convert Source Target where Source ~> Target { convert: Source -> Target }\n",
        "impl Bound I32 { def check = value => True }\n",
        "impl <T where Bound T> Convert T T {\n",
        "    def convert = value => value\n",
        "}\n",
        "let result: I32 = Convert.convert (5 satisfies I32)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a generic impl of a multi-parameter functional-dependency trait should dispatch and compile");
}

#[test]
fn list_supports_bracket_indexing_mutation_and_iteration() {
    let module = type_check(concat!(
        "let mut values: List I32 = List.new ()\n",
        "List.push values 10\n",
        "List.push values 20\n",
        "List.push values 30\n",
        "let index: USize = 1 satisfies USize\n",
        "let read: I32 = values[index]\n",
        "values[index] = 99\n",
        "let mut sum: I32 = 0\n",
        "for item in values {\n",
        "  sum = sum + item\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("List Index/MutateIndex/Iterator/IntoIterator should compile");
}

#[test]
fn list_of_macro_builds_a_list_from_values() {
    let module = type_check(concat!(
        "let empty: List I32 = List.of ()\n",
        "let one: List I32 = List.of (42)\n",
        "let many: List I32 = List.of (1, 2, 3, 4, 5)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        "def push_value: (mut Buffer I32, I32) -> () = (buffer, value) => {\n",
        "  Buffer.push buffer value\n",
        "}\n",
        "let mut values: Buffer I32 = Buffer.with_capacity (2 satisfies USize)\n",
        "push_value (values, 10)\n",
        "push_value (values, 20)\n",
        "let length: USize = Buffer.length values\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("wrapped curried Buffer.push should compile");
}

#[test]
fn validates_the_standard_library_string_representation() {
    let valid_diagnostics = string_contract_diagnostics(concat!(
        "pub type String = Slice U8\n",
        "def exposed_bytes: String -> Slice U8 = String value => value\n",
        "def matched_bytes: String -> Slice U8 = value => match value { String bytes => bytes, }\n",
    ));
    assert!(
        valid_diagnostics.is_empty(),
        "valid String contract diagnostics: {valid_diagnostics:?}"
    );

    for (declaration, expected) in [
        (
            "pub type String = opaque\n",
            "standard library type `String` must be a represented distinct type",
        ),
        (
            "pub(repr) type String = Slice U8\n",
            "standard library type `String` must keep its representation private",
        ),
        (
            "pub type String T = Slice U8\n",
            "standard library type `String` must not accept compile-time arguments",
        ),
        (
            "pub type String = Slice I8\n",
            "standard library type `String` must be represented by `Slice U8`, found `Slice I8`",
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
        "  let copied: String = CString.to_string c_text\n",
        "  let converted: CString = CString.from_string text\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("String operations should compile");

    assert!(llvm.contains("string.pointer"));
    assert!(llvm.contains("string.length"));
    assert!(!llvm.contains("string.capacity"));
    assert!(!llvm.contains("string.allocate"));
    assert!(llvm.contains("@strlen"));
    assert!(llvm.contains("@memchr"));
    assert!(llvm.contains("@llvm.trap"));
    assert!(llvm.contains("c\"hello\\00\""));
}

#[test]
fn c_string_rejects_non_literal_arguments() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("c_string should require a literal during expansion");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`c_string` requires a string literal")
    );
}

#[test]
fn c_string_rejects_interior_nul_bytes() {
    let module = type_check("use std.cinterop.*\nc_string \"bad\\0value\"\n");
    let context = Context::create();
    let diagnostics = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect_err("interior NUL should fail code generation");
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
fn inspects_identifier_and_call_syntax() {
    let module = type_check(concat!(
        "macro call_argument = value: CallExpr => match value {\n",
        "    CallExpr (_, argument) => argument,\n",
        "}\n",
        "macro call_callee = value: CallExpr => value.callee\n",
        "macro classify_ident = value: Ident String => match value {\n",
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
        .compile_module(&module)
        .expect("structured syntax inspection should generate code");
}

#[test]
fn constructs_identifier_and_call_syntax() {
    let module = type_check(concat!(
        "macro generated_name = _: Expr => Ident \"answer\"\n",
        "macro generated_call = callee: Expr => argument: Expr =>\n",
        "    CallExpr (callee: callee, argument: argument)\n",
        "def identity = (value: I32) => value\n",
        "let answer: I32 = 40\n",
        "let name: I32 = generated_name ()\n",
        "let call: I32 = generated_call identity 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("bare `Ident` should default its spelling to `String` and generate code");
}

#[test]
fn bare_ident_and_explicit_ident_string_are_the_same_type() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("narrow syntax nodes are not supported parse_quote contexts");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "Ident String is not a supported `parse_quote` context"
    }));
}

#[test]
fn structured_syntax_overloads_use_leaf_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Expr => parse_quote { 1 }\n",
        "macro classify = _: CallExpr => parse_quote { 2 }\n",
        "macro classify = _: UnstructuredExpr => parse_quote { 3 }\n",
        "macro classify_name = _: Expr => parse_quote { 4 }\n",
        "macro classify_name = _: Ident String => parse_quote { 5 }\n",
        "let call: I32 = classify (f 0)\n",
        "let other: I32 = classify 0\n",
        "let name: I32 = classify_name identifier\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("structured syntax overloads should select a unique leaf");
}

#[test]
fn constructed_identifiers_use_definition_hygiene_and_children_keep_caller_hygiene() {
    let module = type_check(concat!(
        "macro definition_name = _: Expr => Ident \"captured\"\n",
        "macro apply = callee: Expr => argument: Expr =>\n",
        "    CallExpr (callee: callee, argument: argument)\n",
        "let captured: I32 = 7\n",
        "def definition_site = (captured: String) => definition_name ()\n",
        "def caller_site = (local: (I32 -> I32)) => apply local 8\n",
        "let first: I32 = definition_site \"shadow\"\n",
        "let second: I32 = caller_site (value => value)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("constructed syntax should retain the appropriate hygiene context");
}

#[test]
fn mutates_call_syntax_with_value_semantics_and_shared_capture_cells() {
    let module = type_check(concat!(
        "macro replace_call = value: CallExpr => replacement: Expr => {\n",
        "    let original = value\n",
        "    let mut changed = value\n",
        "    changed.argument = replacement\n",
        "    parse_quote { ($original, $changed) }\n",
        "}\n",
        "macro replace_from_closure = value: CallExpr => replacement: Expr => {\n",
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
    let stapler::Expression::Product(pair) = pair else {
        panic!("replacement macro should expand to a product");
    };
    let arguments = pair
        .elements
        .iter()
        .map(|element| {
            let stapler::Expression::Call(call) = &element.value else {
                panic!("pair element should remain a call");
            };
            let stapler::Expression::Integer(argument) = call.argument.as_ref() else {
                panic!("call argument should be an integer");
            };
            argument.literal.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(arguments, ["1", "2"]);

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("mutated syntax should generate code");
}

#[test]
fn diagnoses_invalid_structured_syntax_operations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            .expect_err("invalid structured syntax operation should fail expansion");
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "let generated: CallExpr\n",
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
            .expect_err("runtime syntax values should fail expansion");
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
        .compile_module(&module)
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
        "macro @value = value => _ => parse_quote { let generated: I32 = $value }\n",
        "macro @typed: Parenthesized (Type) -> Item -> Item = ty => _ => parse_quote { let typed: $ty = 1 }\n",
        "macro @bind: Parenthesized (Pattern) -> Item -> Item = pattern => _ => parse_quote { let $pattern = (40, 2) }\n",
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
        .compile_module(&module)
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
        .compile_module(&module)
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
        "macro @split: Item -> Sequence Item = _ => parse_quote {\n",
        "    let first: I32 = 1\n",
        "    let second: I32 = 2\n",
        "}\n",
        "@split\n",
        "let original: I32 = 0\n",
        "let result: I32 = first + second\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("modifier producing raw Syntax that reparses to multiple items should compile");
}

#[test]
fn modifier_macro_deletes_its_target_by_producing_zero_items() {
    let module = type_check(concat!(
        "macro @delete: Item -> Sequence Item = _ => parse_quote {}\n",
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
        "macro @split: Item -> Sequence Item = _ => parse_quote {\n",
        "    let first: I32 = 20\n",
        "    let second: I32 = 22\n",
        "}\n",
        "macro @delete: Item -> Sequence Item = _ => parse_quote {}\n",
        "def answer: () -> I32 = () => {\n",
        "    @split let original = 0\n",
        "    @delete let removed = 0\n",
        "    first + second\n",
        "}\n",
        "let result: I32 = answer ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("block modifier output should splice and compile");
}

#[test]
fn block_modifiers_may_generate_supported_declarations() {
    let module = type_check(concat!(
        "mod helpers { pub def value: I32 = 42 }\n",
        "macro @declarations: Item -> Sequence Item = _ => parse_quote {\n",
        "    type alias Local = I32\n",
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
        .compile_module(&module)
        .expect("supported declarations generated in a block should compile");
}

#[test]
fn block_modifiers_reject_unsupported_and_public_outputs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            .expect_err("invalid block modifier output should be rejected");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message == "item is not supported in a block expression"
            }),
            "expected unsupported block item diagnostic, found {diagnostics:#?}",
        );
    }
}

#[test]
fn non_outermost_modifier_applies_the_next_modifier_to_its_first_item() {
    let module = type_check(concat!(
        "macro @outer: Item -> Item = _ => parse_quote { let transformed: I32 = 100 }\n",
        "macro @inner: Item -> Sequence Item = _ => parse_quote {\n",
        "    let first: I32 = 1\n",
        "    let second: I32 = 2\n",
        "}\n",
        "@outer\n",
        "@inner\n",
        "let original: I32 = 0\n",
        "let result: I32 = transformed + second\n",
    ));
    assert!(!module.resolved().syntax().items.iter().any(|item| {
        matches!(item, item
            if matches!(item, Item::Binding(binding)
                if binding.name == "first"))
    }));
    let context = Context::create();
    CodeGenerator::new(&context).compile_module(&module).expect(
        "the outermost modifier should apply to the inner modifier's first item, with the \
             rest passed through unmodified",
    );
}

#[test]
fn outermost_modifier_combines_its_own_items_with_earlier_trailing_items() {
    let module = type_check(concat!(
        "macro @outer: Item -> Sequence Item = _ => parse_quote {\n",
        "    let replaced_first: I32 = 100\n",
        "    let from_outer: I32 = 200\n",
        "}\n",
        "macro @inner: Item -> Sequence Item = _ => parse_quote {\n",
        "    let first: I32 = 1\n",
        "    let second: I32 = 2\n",
        "}\n",
        "@outer\n",
        "@inner\n",
        "let original: I32 = 0\n",
        "let result: I32 = replaced_first + from_outer + second\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context).compile_module(&module).expect(
        "items trailing from an inner modifier should combine with the outermost \
             modifier's own multi-item result",
    );
}

#[test]
fn diagnoses_non_outermost_modifier_producing_zero_items() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = concat!(
        "macro @outer: Item -> Item = item => item\n",
        "macro @inner: Item -> Sequence Item = _ => parse_quote {}\n",
        "@outer\n",
        "@inner\n",
        "let original: I32 = 0\n",
    );
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(&with_syntax_imports(source), root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("a non-outermost modifier producing zero items should be diagnosed");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message
                == "modifier macro `@inner` produced no items, so the remaining modifiers in the chain have nothing to apply to"
        }),
        "expected non-outermost zero-item diagnostic, found {diagnostics:#?}"
    );
}

#[test]
fn expands_visibility_aware_macros_and_contextual_visibility_splices() {
    let module = type_check(concat!(
        "def normalize_visibility: Visibility -> Visibility = value => value\n",
        "def visibility_number: Visibility -> Expr = value => match value {\n",
        "    Private => parse_quote { 1 },\n",
        "    Public => parse_quote { 2 },\n",
        "    PublicRepr => parse_quote { 3 },\n",
        "}\n",
        "macro define_alias = vis: MacroCallVisibility => ty: Type => {\n",
        "    let actual = normalize_visibility vis\n",
        "    parse_quote { $actual type alias Generated = $ty }\n",
        "}\n",
        "macro classify = before: Expr => vis: Visibility => after: Expr => {\n",
        "    let number = visibility_number vis\n",
        "    number\n",
        "}\n",
        "macro call_visibility = vis: MacroCallVisibility => visibility_number vis\n",
        "macro first_visibility = vis: Visibility => value: Expr => visibility_number vis\n",
        "macro final_visibility = value: Expr => vis: Visibility => visibility_number vis\n",
        "pub define_alias I32\n",
        "let implicit: I32 = classify 10 20\n",
        "let public: I32 = classify 10 pub 20\n",
        "let represented: I32 = classify 10 pub(repr) 20\n",
        "let private_call: I32 = call_visibility\n",
        "let first_private: I32 = first_visibility 0\n",
        "let first_public: I32 = first_visibility pub 0\n",
        "let final_private: I32 = final_visibility 0\n",
        "let final_public: I32 = final_visibility 0 pub\n",
        "let generated: Generated = 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("visibility-aware macro output should compile");
}

#[test]
fn expands_standard_typegroup_into_module_variants_and_alias() {
    let module = type_check(concat!(
        "pub(repr) typegroup Pattern {\n",
        "    Literal String,\n",
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
        .compile_module(&module)
        .expect("generated typegroup variants and alias should compile");
}

#[test]
fn resolves_and_merges_type_companion_items() {
    type_check(concat!(
        "type alias Animal = I32\n",
        "let offset: I32 = 1\n",
        "companion Animal { pub def move_to = animal: Animal => animal + offset }\n",
        "companion Animal { pub def stop = animal: Animal => animal }\n",
        "let moved: Animal = Animal.move_to 1\n",
        "let stopped: Animal = Animal.stop moved\n",
    ));
    type_check(concat!(
        "type alias Box T = (value: T)\n",
        "companion<T> Box T { pub def box_identity: Box T -> Box T = box => box }\n",
        "let identity: Box I32 -> Box I32 = Box.box_identity\n",
    ));
}

#[test]
fn type_checks_companion_method_call_syntax() {
    let module = type_check(concat!(
        "type alias Animal = I32\n",
        "companion Animal { pub def move_to: Animal -> (F32, F32) -> Animal = animal => _ => animal }\n",
        "let animal: Animal = 1\n",
        "let moved: Animal = animal^move_to (1.0, 1.0)\n",
        "let partially_applied: (F32, F32) -> Animal = animal^move_to\n",
        "def move: Animal -> Animal = value => value^move_to (1.0, 1.0)\n",
        "def make: () -> Animal = () => animal\n",
        "let moved_from_call: Animal = (make ())^move_to (1.0, 1.0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("companion method calls should lower as ordinary calls");
}

#[test]
fn typegroup_supports_generic_groups_and_reexports_their_variants() {
    let module = type_check(concat!(
        "pub(repr) typegroup Maybe T {\n",
        "    Missing,\n",
        "    Present T,\n",
        "}\n",
        "pub use Maybe.*\n",
        "let missing: Maybe I32 = Missing\n",
        "let present: Maybe I32 = Present 1\n",
        "let qualified: Maybe String = Maybe.Present \"value\"\n",
        "pub(repr) typegroup Either (L, R,) {\n",
        "    Left L,\n",
        "    Right R,\n",
        "}\n",
        "pub(repr) typegroup Mixed A (B, C) D {\n",
        "    Empty,\n",
        "    Value (A, B, C, D),\n",
        "}\n",
        "let value: (I32, String, Bool, F64) = (1, \"two\", True, 4.0)\n",
        "let mixed: Mixed I32 (String, Bool) F64 = Mixed.Value value\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic typegroup variants and their reexports should compile");
}

#[test]
fn equals_and_fat_arrow_are_structured_syntax_nodes() {
    let module = type_check(concat!(
        "macro punctuation = _: Ident String => equal: Equals => _: Ident String => arrow: FatArrow => _: Braced (Sequence SyntaxNode) =>\n",
        "    match (equal, arrow, Equals, FatArrow) {\n",
        "        (Equals, FatArrow, Equals, FatArrow) => parse_quote { let punctuated: I32 = 42 },\n",
        "    }\n",
        "punctuation marker = T => {}\n",
        "let result: I32 = punctuated\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("comma, equals, and fat-arrow parse_quote results should construct");
}

#[test]
fn parse_quote_produces_visibility_result_type() {
    let module = type_check(concat!(
        "macro visibility: Expr -> Expr = _: Expr => {\n",
        "    let none: Visibility = parse_quote { }\n",
        "    let pub_: Visibility = parse_quote { pub }\n",
        "    let repr: Visibility = parse_quote { pub(repr) }\n",
        "    match (none, pub_, repr) {\n",
        "        (Private, Public, PublicRepr) => parse_quote { 42 },\n",
        "    }\n",
        "}\n",
        "let result: I32 = visibility (0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("private, public, and public-repr parse_quote results should construct");
}

#[test]
fn parse_quote_produces_delimited_result_types() {
    let module = type_check(concat!(
        "macro delimited: Expr -> Expr = _: Expr => {\n",
        "    let fixed: Parenthesized (Ident String, Comma, Ident String) = parse_quote { (a, b) }\n",
        "    let sequence: Bracketed (Sequence Ident String) = parse_quote { [one two] }\n",
        "    let separated: Braced (Separated (Ident String) Comma) = parse_quote { { x, y, } }\n",
        "    let nested: Parenthesized Syntax = parse_quote { ((a), (b)) }\n",
        "    match (fixed, sequence, separated) {\n",
        "        (Parenthesized (Ident \"a\", Comma, Ident \"b\"),\n",
        "         Bracketed (Sequence (Ident \"one\", Ident \"two\")),\n",
        "         Braced (Separated (separator: Comma, elements: (Ident \"x\", _), trailing: True()))) =>\n",
        "            match nested { Parenthesized _ => parse_quote { 42 } },\n",
        "    }\n",
        "}\n",
        "let result: I32 = delimited (0)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context).compile_module(&module).expect(
        "fixed, sequence, and separated parse_quote results should construct, including a \
             nested same-kind delimiter captured opaquely as Syntax",
    );
}

#[test]
fn parse_quote_rejects_spliced_delimited_result() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro bad: Expr -> Expr = value: Expr => {\n",
                "    let x: Parenthesized (Expr, Expr) = parse_quote { ($value, $value) }\n",
                "    parse_quote { 0 }\n",
                "}\n",
                "let result: I32 = bad (1)\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("splicing into a delimited-result quotation should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .starts_with("quotation cannot be interpreted as Parenthesized")
    }));
}

#[test]
fn parse_quote_rejects_spliced_visibility_result() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("splicing a whole Visibility value should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("contains visibility syntax, not expression syntax")
    }));
}

#[test]
fn rejects_legacy_typegroup_call_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            .expect_err("legacy typegroup syntax should not expand");
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("unqualified variants should require an explicit use");
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
        "    Pair (I32, String),\n",
        "    Empty,\n",
        "}\n",
        "pub(repr) typegroup Generic {\n",
        "    Wrapped Option I32,\n",
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
        .compile_module(&module)
        .expect("private and compound typegroups should compile");
}

#[test]
fn public_repr_is_allowed_on_singleton_types() {
    type_check("pub(repr) type Unit\nlet unit: Unit = Unit\n");
}

#[test]
fn rejects_empty_typegroups_and_top_level_optional_macro_parameters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "typegroup Empty {}\n",
            "compile-time match was not exhaustive",
        ),
        (
            "macro invalid: Optional Type -> Expr = value => parse_quote { 0 }\ninvalid I32\n",
            "`Optional` and product syntax shapes may only appear inside delimited contents",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid typegroup infrastructure use should fail");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn public_repr_macro_call_can_generate_a_public_representation() {
    let module = type_check(concat!(
        "macro define_box = vis: MacroCallVisibility => parse_quote { $vis type Box = I32 }\n",
        "pub(repr) define_box\n",
        "let boxed: Box = Box 42\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::TypeDeclaration(declaration)
            if declaration.name == "Box"
                && declaration.visibility == stapler::Visibility::Public
                && declaration.representation_visibility == stapler::Visibility::Public)
    }));
}

#[test]
fn modifiers_compose_after_visibility_aware_item_calls() {
    let module = type_check(concat!(
        "macro @identity: Item -> Item = item => item\n",
        "macro define = vis: MacroCallVisibility => parse_quote { $vis let generated: I32 = 42 }\n",
        "@identity\n",
        "pub define\n",
        "let result: I32 = generated\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, item
            if matches!(item, Item::Binding(binding)
                if binding.name == "generated"
                    && binding.visibility == stapler::Visibility::Public))
    }));
}

#[test]
fn diagnoses_invalid_visibility_macro_uses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro invalid: Expr -> MacroCallVisibility -> Expr = left => vis => left\n",
            "macro `invalid` may use `MacroCallVisibility` only as its first parameter",
        ),
        (
            "def invalid: MacroCallVisibility -> Visibility = value => value\n",
            "`MacroCallVisibility` may only be the first parameter of a function-style macro",
        ),
        (
            "macro ordinary: Expr -> Expr = value => parse_quote { $value }\npub ordinary 1\n",
            "macro `ordinary` has no overload whose first parameter is `MacroCallVisibility`",
        ),
        (
            "macro invalid = vis: MacroCallVisibility => parse_quote { $vis type alias Generated = I32 }\npub(repr) invalid\n",
            "`PublicRepr` visibility requires a represented distinct type",
        ),
        (
            "macro @identity: Item -> Item = item => item\nmacro expression = vis: MacroCallVisibility => parse_quote { 1 }\n@identity\npub expression\n",
            "modifier macros may only be applied to `let`, `def`, `type`, `extern`, `trait`, or `impl` items",
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
            .expect_err("invalid visibility syntax should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn implicit_macro_call_visibility_can_make_overloads_ambiguous() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = concat!(
        "macro choose: Expr -> Expr = value => parse_quote { $value }\n",
        "macro choose: MacroCallVisibility -> Expr -> Expr = vis => value => parse_quote { $value }\n",
        "let result = choose 1\n",
    );
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(&with_syntax_imports(source), root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("equal-consumption overloads should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `choose`")
    );
}

#[test]
fn diagnoses_invalid_modifier_definitions_and_applications() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro @invalid: SyntaxNode -> Item -> Item = value => item => item\n",
            "modifier macro `@invalid` must have signature `Item -> Item`, `Item -> Sequence Item`, or `Item -> Syntax`, optionally with a leading `Parenthesized (Expr | Type | Pattern) ->` argument",
        ),
        (
            "macro @required: Parenthesized (Expr) -> Item -> Item = value => item => item\n@required\nlet value = 1\n",
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
            "@doc\ntype Documented = I32\n",
            "`@doc` requires a parenthesized string literal",
        ),
        (
            "@doc(42)\ntype Documented = I32\n",
            "`@doc` requires a string literal argument",
        ),
        (
            "@doc(\"bad\\q\")\ntype Documented = I32\n",
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
            "@recursive_constructor\ntype Box = I32\n",
            "`@recursive_constructor` may only mark a compiler-owned recursive constructor",
        ),
        (
            "@recursive_constructor(1)\ntype Box = I32\n",
            "`@recursive_constructor` does not accept an argument",
        ),
        (
            "@recursive_constructor\nlet value = 1\n",
            "`@recursive_constructor` may only modify a type declaration",
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
            .expect_err("invalid modifier use should fail expansion");
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
        "macro define_value = ty: Type => value: Expr => {\n",
        "    let actual = type_identity ty\n",
        "    parse_quote { let generated: $actual = $value }\n",
        "}\n",
        "macro destructure = pattern: Pattern => value: Expr => {\n",
        "    let actual = pattern_identity pattern\n",
        "    parse_quote { let $actual = $value }\n",
        "}\n",
        "define_value (I32 -> I32) (value => value)\n",
        "destructure ((left, right)) (40, 2)\n",
        "let answer: I32 = generated (left + right)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("type and pattern splices should generate valid code");
}

#[test]
fn accepts_product_type_and_pattern_macro_inputs_without_extra_grouping() {
    let module = type_check(concat!(
        "macro for = pattern: Pattern => _: Ident \"in\" => value: Expr => body: Expr => parse_quote {\n",
        "    { let $pattern = $value; $body }\n",
        "}\n",
        "macro ascribe = ty: Type => value: Expr => parse_quote { $value satisfies $ty }\n",
        "let direct: I32 = for (left, right) in (40, 2) { left + right }\n",
        "let legacy: I32 = for ((left, right)) in (40, 2) { left + right }\n",
        "let empty: () = for () in () { () }\n",
        "let pair: (I32, String) = ascribe (I32, String) (1, \"value\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("product category arguments should not require extra grouping");
}

#[test]
fn splices_types_and_patterns_through_expression_quotation_contexts() {
    let module = type_check(concat!(
        "macro typed_identity = ty: Type => parse_quote { (value => value) satisfies ($ty -> $ty) }\n",
        "macro parameter_function = pattern: Pattern => parse_quote { $pattern => 42 }\n",
        "macro matching = pattern: Pattern => parse_quote { match (True satisfies Bool) { $pattern => 1, _ => 0 } }\n",
        "let identity: I32 -> I32 = typed_identity I32\n",
        "let constant: I32 -> I32 = parameter_function (_)\n",
        "let matched: I32 = matching True\n",
        "let result: I32 = identity (constant matched)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("contextual type and pattern splices should compile in expressions");
}

#[test]
fn quote_uses_contextual_results_and_reinterprets_opaque_fragments() {
    let module = type_check(concat!(
        "macro delayed: Expr -> Expr = value => {\n",
        "    let fragment: Syntax = quote { $value + 1 }\n",
        "    parse_quote { $fragment } satisfies Expr\n",
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
        .compile_module(&module)
        .expect("opaque syntax should reparse according to its later quote context");
}

#[test]
fn quote_never_validates_or_reinterprets_its_result_unlike_parse_quote() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    parse_quote { let generated = 1 } satisfies Expr\n",
                "}\n",
                "let result = invalid ()\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("an item template is not a valid `Expr`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "quotation cannot be interpreted as Expr" })
    );

    resolve(concat!(
        "macro always_syntax: Expr -> Expr = _: Expr => {\n",
        "    let fragment: Syntax = quote { let generated = 1 } satisfies Expr\n",
        "    parse_quote { 0 }\n",
        "}\n",
        "let result = always_syntax ()\n",
    ));
}

#[test]
fn quote_result_excludes_syntax_which_remains_quotes_alone() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    for source in [
        concat!(
            "macro invalid: Expr -> Expr = value => {\n",
            "    let fragment: Syntax = parse_quote { $value }\n",
            "    parse_quote { $fragment } satisfies Expr\n",
            "}\n",
            "let result = invalid (1)\n",
        ),
        concat!(
            "macro invalid = value => parse_quote { $value } satisfies Syntax\n",
            "let result = invalid (1)\n",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("`Syntax` is `quote`'s result, not a `parse_quote` target");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.message
                == "Syntax is not a supported `parse_quote` context"),
            "{diagnostics:#?}",
        );
    }
}

#[test]
fn parse_quote_without_a_contextual_type_is_an_error() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("an untyped helper leaves `parse_quote` without a contextual type");
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("macro invalid: Expr -> Expr = value => quote { $value }\n"),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("`quote` always returns `Syntax`, not the declared `Expr` result");
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
        "macro capture: Braced Syntax -> Expr = body => match body {\n",
        "    Braced fragment => parse_quote { $fragment } satisfies Expr,\n",
        "}\n",
        "macro choose: Syntax -> Expr = _: Syntax => parse_quote { 1 }\n",
        "macro choose: SyntaxNode -> Expr = _: SyntaxNode => parse_quote { 2 }\n",
        "macro accepts: Braced Syntax -> Expr = _: Braced Syntax => parse_quote { 3 }\n",
        "let captured: I32 = capture { 40 + 2 }\n",
        "let selected: I32 = choose name\n",
        "let arbitrary: I32 = accepts { left, => right }\n",
        "let result: I32 = captured + selected + arbitrary\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("whole-fragment capture and SyntaxNode overload precedence should compile");
}

#[test]
fn syntax_node_quotation_requires_one_shortest_structural_node() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("two shortest nodes must not satisfy SyntaxNode");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "quotation does not contain exactly one structural syntax node"
    }));
}

#[test]
fn contextual_item_sequence_quotes_flatten_empty_single_and_multiple_results() {
    let module = type_check(concat!(
        "macro empty: Expr -> Sequence Item = _: Expr => parse_quote {}\n",
        "macro single: Expr -> Sequence Item = _: Expr => parse_quote { let one: I32 = 1 }\n",
        "macro multiple: Expr -> Sequence Item = _: Expr => parse_quote {\n",
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
        .compile_module(&module)
        .expect("contextual item sequences should flatten in source order");
}

#[test]
fn generic_user_macros_and_unsupported_parse_quote_contexts_are_rejected() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("macro generic: <T> Expr -> T = value => value\n", root)
        .expect("generic macro syntax should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("generic user macros remain unsupported");
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message == "generic user-defined macros are not supported"));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    let ident: Ident String = parse_quote { name }\n",
                "    parse_quote { 0 }\n",
                "}\n",
                "let result = invalid ()\n",
            ),
            root,
        )
        .expect("unsupported contextual quote source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("narrow syntax nodes are not supported parse_quote contexts");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "Ident String is not a supported `parse_quote` context"
    }));
}

#[test]
fn type_and_pattern_macro_inputs_support_atomic_and_compound_forms() {
    let module = type_check(concat!(
        "macro atomic_type = _: Type => parse_quote { let atomic: I32 = 1 }\n",
        "macro applied_type = _: Type => parse_quote { let applied: I32 = 2 }\n",
        "macro nominal_pattern = _: Pattern => parse_quote { let nominal: I32 = 3 }\n",
        "atomic_type I32\n",
        "applied_type (Ref I32)\n",
        "nominal_pattern (Some value)\n",
        "let result: I32 = atomic + applied + nominal\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("atomic and grouped category arguments should compile");
}

#[test]
fn type_and_pattern_overlaps_with_expression_overloads_are_ambiguous() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            .expect_err("overlapping syntax categories should be ambiguous");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `choose`")
        );
    }
}

#[test]
fn diagnoses_invalid_type_and_pattern_macro_inputs_and_splices() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            .expect_err("invalid category syntax should fail expansion or resolution");
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
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("nested macros in generated items should expand");
}

#[test]
fn generates_extern_trait_and_implementation_items() {
    let module = type_check(concat!(
        "macro define_extern = _: Expr => parse_quote {\n",
        "    extern \"c\" { let generated_external: I32 -> I32 }\n",
        "}\n",
        "macro define_trait = _: Expr => parse_quote {\n",
        "    trait GeneratedTrait T { transform: T -> T }\n",
        "}\n",
        "macro define_impl = replacement: Expr => parse_quote {\n",
        "    impl GeneratedTrait I32 { def transform = value => $replacement }\n",
        "}\n",
        "define_extern ()\n",
        "define_trait ()\n",
        "define_impl 41\n",
        "let transformed: I32 = GeneratedTrait.transform 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generated resolver-safe declaration items should generate code");
}

#[test]
fn generates_generic_conditional_trait_implementation_items() {
    let module = type_check(concat!(
        "trait Bound T { check: T -> Bool }\n",
        "trait Target T { act: T -> T }\n",
        "impl Bound I32 { def check = value => True }\n",
        "macro define_impl = _: Expr => parse_quote {\n",
        "    impl <T where Bound T> Target T { def act = value => value }\n",
        "}\n",
        "define_impl ()\n",
        "let answer: I32 = Target.act 41\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a macro-generated conditional trait implementation should generate code");
}

#[test]
fn generates_traits_with_functional_dependencies() {
    let module = type_check(concat!(
        "macro define_trait = _: Expr => parse_quote {\n",
        "    trait Generated Input Output where Input ~> Output { generate: Input -> Output }\n",
        "}\n",
        "define_trait ()\n",
        "impl Generated I32 String { def generate = value => \"generated\" }\n",
        "let generated = Generated.generate 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generated functional dependencies should resolve and specialize");
}

#[test]
fn diagnoses_invalid_item_macro_outputs_and_placements() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            .expect_err("invalid item macro use should fail expansion");
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
        "macro choose = condition => then => else => parse_quote {\n",
        "    match $condition { True => $then, False => $else, }\n",
        "}\n",
        "macro passthrough = value => syntax_identity value\n",
        "let condition: Bool = True\n",
        "let result: I32 = passthrough (choose condition 1 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("expanded macros should generate code");
}

#[test]
fn expands_typed_macros_with_literal_identifier_parameters() {
    let module = type_check(concat!(
        "macro conditional =\n",
        "    condition: Expr =>\n",
        "    then_branch: Expr =>\n",
        "    _: Ident \"else\" =>\n",
        "    else_branch: Expr => parse_quote {\n",
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
        .compile_module(&module)
        .expect("typed macro should expand to an ordinary expression");
}

#[test]
fn expands_macros_with_delimited_syntax_parameters() {
    let module = type_check(concat!(
        "macro pair = _: Parenthesized (Ident String, Ident String) => parse_quote { 11 }\n",
        "macro names = _: Bracketed (Sequence Ident String) => parse_quote { 22 }\n",
        "macro body = _: Braced (Sequence SyntaxNode) => parse_quote { 33 }\n",
        "let pair_result: I32 = pair (left right)\n",
        "let empty_names: I32 = names []\n",
        "let names_result: I32 = names [one two three]\n",
        "let body_result: I32 = body { left (right nested) }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("delimited macro parameters should expand");
}

#[test]
fn delimited_macro_overloads_use_structural_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Expr => parse_quote { 1 }\n",
        "macro classify = _: Parenthesized (Sequence SyntaxNode) => parse_quote { 2 }\n",
        "macro classify = _: Parenthesized (Sequence Ident String) => parse_quote { 3 }\n",
        "macro classify = _: Parenthesized (Ident String, Ident String) => parse_quote { 4 }\n",
        "let fixed: I32 = classify (left right)\n",
        "let sequence: I32 = classify (left)\n",
        "let empty: I32 = classify ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("delimiter overloads should be ordered by structural specificity");
}

#[test]
fn constructs_and_destructures_delimited_syntax_values() {
    let module = type_check(concat!(
        "macro inspect = value: Parenthesized (Sequence Ident String) => match value {\n",
        "    Parenthesized (Sequence (Ident \"only\")) => parse_quote { 1 },\n",
        "    _ => parse_quote { 0 },\n",
        "}\n",
        "macro construct = _: Expr => Parenthesized (parse_quote { 7 })\n",
        "macro construct_empty = _: Expr => Parenthesized (Sequence ())\n",
        "macro construct_sequence = _: Expr => Parenthesized (Sequence (parse_quote { increment }, parse_quote { 41 }))\n",
        "macro construct_braced = _: Expr => Braced (Sequence (parse_quote { 9 }))\n",
        "macro preserve = value: Parenthesized (Sequence SyntaxNode) => value\n",
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
        .compile_module(&module)
        .expect("delimiter values should support constructors and nominal patterns");
}

#[test]
fn rejects_invalid_sequence_positions_and_source_punctuation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Sequence Ident => parse_quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("bare Sequence should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "a top-level `Sequence` parameter must be followed by a parameter that always consumes source syntax"
    }));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Separated (Ident String) Comma => parse_quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("bare Separated should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "`Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`"
    }));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Braced (Sequence Syntax) => parse_quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("opaque Syntax has no canonical sequence partition");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .starts_with("opaque `Syntax` may be a top-level argument")
    }));
}

#[test]
fn top_level_macro_sequences_capture_zero_one_and_many_arguments() {
    let module = type_check(concat!(
        "macro count = _: Ident \"marker\" => values: Sequence (Ident String) => _: Equals => name: Ident String => _: FatArrow => _: Braced Syntax => match values {\n",
        "    Sequence () => quote { let $name: I32 = 0 },\n",
        "    Sequence (first: Ident String, rest: Sequence Ident String) => match rest {\n",
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
        .compile_module(&module)
        .expect("top-level macro sequences should compile");
}

#[test]
fn top_level_macro_sequences_backtrack_for_visibility_and_fixed_suffixes() {
    let module = type_check(concat!(
        "macro classify = _: Ident \"marker\" =>\n",
        "    values: Sequence (Ident String) =>\n",
        "    visibility: Visibility =>\n",
        "    _: Equals =>\n",
        "    name: Ident String => _: FatArrow => _: Braced Syntax => match (values, visibility) {\n",
        "        (Sequence (), Private) => quote { let $name: I32 = 40 },\n",
        "        (Sequence (first: Ident String, rest: Sequence Ident String), Public) => quote { let $name: I32 = 41 },\n",
        "        (Sequence (first: Ident String, rest: Sequence Ident String), PublicRepr) => quote { let $name: I32 = 42 },\n",
        "        _ => quote { let $name: I32 = 43 },\n",
        "    }\n",
        "classify marker = private_value => {}\n",
        "classify marker alpha beta pub = public_value => {}\n",
        "classify marker alpha pub(repr) = public_repr_value => {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("sequence suffix matching should handle implicit and explicit visibility");
}

#[test]
fn fixed_and_more_specific_overloads_beat_top_level_sequences() {
    let module = type_check(concat!(
        "macro choose = _: Sequence Expr => _: Equals => name: Ident String => _: FatArrow => _: Braced Syntax => quote { let $name: String = \"wrong\" }\n",
        "macro choose = _: Sequence (Ident String) => _: Equals => name: Ident String => _: FatArrow => _: Braced Syntax => quote { let $name: I32 = 1 }\n",
        "macro choose = _: Ident String => _: Equals => name: Ident String => _: FatArrow => _: Braced Syntax => quote { let $name: I32 = 2 }\n",
        "choose value = fixed => {}\n",
        "choose left right = repeated => {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("fixed and category-specific overloads should win");
}

#[test]
fn annotated_top_level_sequences_compile_and_incomparable_sequences_are_ambiguous() {
    let module = type_check(concat!(
        "macro annotated: Sequence (Ident String) -> Equals -> Ident String -> FatArrow -> Braced Syntax -> Syntax =\n",
        "    values: Sequence (Ident String) => _: Equals => name: Ident String => _: FatArrow => _: Braced Syntax => quote { let $name: I32 = 42 }\n",
        "annotated first second = generated => {}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("annotated top-level sequences should compile");

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro clash = _: Sequence Type => _: Equals => _: Ident String => _: FatArrow => _: Braced Syntax => quote { let generated: I32 = 1 }\n",
                "macro clash = _: Sequence Pattern => _: Equals => _: Ident String => _: FatArrow => _: Braced Syntax => quote { let generated: I32 = 2 }\n",
                "clash Value = output => {}\n",
            )),
            root,
        )
        .expect("ambiguous macro source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("incomparable repeated categories should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `clash`")
    );
}

#[test]
fn rejects_invalid_top_level_macro_sequence_signatures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro invalid = values: Sequence (Ident String) => parse_quote { 0 }\n",
            "a top-level `Sequence` parameter must be followed by a parameter that always consumes source syntax",
        ),
        (
            "macro invalid = first: Sequence (Ident String) => second: Sequence Expr => _: Equals => parse_quote { 0 }\n",
            "a macro signature may contain at most one top-level `Sequence` parameter",
        ),
        (
            "macro invalid = values: Sequence (Ident String) => _: Visibility => parse_quote { 0 }\n",
            "a top-level `Sequence` parameter must be followed by a parameter that always consumes source syntax",
        ),
        (
            "macro @invalid: Sequence (Ident String) -> Item -> Item = values => item => item\n",
            "top-level `Sequence` parameters are not supported by modifier macros",
        ),
        (
            "macro invalid = values: Sequence (Sequence (Ident String)) => _: Equals => parse_quote { 0 }\n",
            "a top-level `Sequence` element must be a single syntax category, found `Sequence Ident String`",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("invalid macro declaration should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid top-level sequence should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected {expected:?}, found {diagnostics:#?}",
        );
    }
}

#[test]
fn matches_comma_and_separated_delimited_syntax() {
    let module = type_check(concat!(
        "macro fixed = _: Parenthesized (Ident String, Comma, Ident String) => parse_quote { 1 }\n",
        "macro separated = _: Parenthesized (Separated (Ident String) Comma) => parse_quote { 2 }\n",
        "macro bracketed = _: Bracketed (Separated (Ident String) Comma) => parse_quote { 5 }\n",
        "macro braced = _: Braced (Separated (Ident String) Comma) => parse_quote { 6 }\n",
        "macro syntax = value: Parenthesized (Sequence SyntaxNode) => match value {\n",
        "    Parenthesized (Sequence (Ident \"left\", Comma, Ident \"right\")) => parse_quote { 3 },\n",
        "    _ => parse_quote { 0 },\n",
        "}\n",
        "macro comma = value: Parenthesized (Comma) => match value {\n",
        "    Parenthesized Comma => parse_quote { 4 },\n",
        "}\n",
        "let fixed_result: I32 = fixed (left, right)\n",
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
        .compile_module(&module)
        .expect("comma and separated syntax should match");
}

#[test]
fn separated_overloads_use_structural_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Parenthesized (Sequence SyntaxNode) => parse_quote { 1 }\n",
        "macro classify = _: Parenthesized (Separated (Ident String) Comma) => parse_quote { 2 }\n",
        "macro classify = _: Parenthesized (Ident String, Comma, Ident String) => parse_quote { 3 }\n",
        "let fixed: I32 = classify (one, two)\n",
        "let separated: I32 = classify (one, two, three)\n",
        "let empty: I32 = classify ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("fixed, separated, and ordinary sequence overloads should be ordered");
}

#[test]
fn constructs_and_destructures_separated_syntax() {
    let module = type_check(concat!(
        "macro inspect = value: Parenthesized (Separated (Ident String) Comma) => match value {\n",
        "    Parenthesized (Separated (separator: Comma, elements: (Ident \"one\", Ident \"two\"), trailing: True())) => parse_quote { 1 },\n",
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
        .compile_module(&module)
        .expect("separated syntax should support construction and patterns");
}

#[test]
fn rejects_malformed_separated_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for argument in ["(,)", "(, one)", "(one,, two)", "(one two)", "(one, 2)"] {
        let source = format!(
            "macro names = _: Parenthesized (Separated (Ident String) Comma) => parse_quote {{ 0 }}\nlet value: I32 = names {argument}\n"
        );
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, root)
            .expect("source should parse");
        NameResolver::new()
            .resolve_program(program)
            .expect_err("malformed separated syntax should be rejected");
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
        .expect_err("an empty separated value cannot have a trailing comma");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "an empty `Separated` value cannot have a trailing separator"
    }));
}

#[test]
fn expands_standard_braced_if_clauses() {
    let module = type_check(concat!(
        "let condition: Bool = True\n",
        "let clauses: I32 = if {\n",
        "  False => 7,\n",
        "  condition => 8,\n",
        "  else => 9,\n",
        "}\n",
        "let clauses_without_else = if { False => () }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("standard braced if should expand and generate code");
}

#[test]
fn requires_else_to_be_the_last_braced_if_clause() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let invalid = if { else => (), True => () }\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("a non-final else clause should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "compile-time match was not exhaustive" })
    );
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
        .compile_module(&module)
        .expect("standard while should expand and generate valid LLVM");
    assert!(llvm.contains("loop.body"));
    assert!(llvm.contains("loop.exit"));
}

#[test]
fn expands_standard_for_over_ranges_and_product_iterators() {
    let module = type_check(concat!(
        "pub(repr) type PairIterator = (current: I32, end: I32)\n",
        "impl Iterator PairIterator (I32, I32) {\n",
        "  def next = PairIterator (current, end) => match current < end {\n",
        "    True() => IterStep.Yield ((current, current + 10), PairIterator (current + 1, end)),\n",
        "    False() => IterStep.Done (PairIterator (current, end)),\n",
        "  }\n",
        "}\n",
        "impl IntoIterator PairIterator PairIterator { def into_iterator = iterator => iterator }\n",
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
        .compile_module(&module)
        .expect("for loops over ranges and custom iterators should generate LLVM");
    assert!(llvm.contains("loop.body"));
    assert!(llvm.contains("trait.call"));
}

#[test]
fn provides_integer_range_iterator_implementations() {
    type_check(concat!(
        "let i8: IterStep (Range I8, I8) = Iterator.next ((0 satisfies I8) .. (1 satisfies I8))\n",
        "let i16: IterStep (Range I16, I16) = Iterator.next ((0 satisfies I16) .. (1 satisfies I16))\n",
        "let i32: IterStep (Range I32, I32) = Iterator.next (0 .. 1)\n",
        "let i64: IterStep (Range I64, I64) = Iterator.next ((0 satisfies I64) .. (1 satisfies I64))\n",
        "let u8: IterStep (Range U8, U8) = Iterator.next ((0 satisfies U8) .. (1 satisfies U8))\n",
        "let u16: IterStep (Range U16, U16) = Iterator.next ((0 satisfies U16) .. (1 satisfies U16))\n",
        "let u32: IterStep (Range U32, U32) = Iterator.next ((0 satisfies U32) .. (1 satisfies U32))\n",
        "let u64: IterStep (Range U64, U64) = Iterator.next ((0 satisfies U64) .. (1 satisfies U64))\n",
        "let isize: IterStep (Range ISize, ISize) = Iterator.next ((0 satisfies ISize) .. (1 satisfies ISize))\n",
        "let usize: IterStep (Range USize, USize) = Iterator.next ((0 satisfies USize) .. (1 satisfies USize))\n",
        "let inclusive: IterStep (RangeInclusive U8, U8) = Iterator.next ((255 satisfies U8) ..= (255 satisfies U8))\n",
    ));
}

#[test]
fn macro_overloads_choose_longest_then_most_specific() {
    let module = type_check(concat!(
        "macro select = value: Expr => parse_quote { 10 }\n",
        "macro select = value: Expr => _: Ident \"with\" => replacement: Expr => parse_quote { $replacement }\n",
        "macro classify = value: SyntaxNode => parse_quote { 1 }\n",
        "macro classify = value: Expr => parse_quote { 2 }\n",
        "macro classify = value: Ident String => parse_quote { 3 }\n",
        "macro classify = _: Ident \"else\" => parse_quote { 4 }\n",
        "let longest: I32 = select 0 with 20\n",
        "let specific: I32 = classify else\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("selected overloads should generate code");
}

#[test]
fn rejects_legacy_standard_if_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "let condition: Bool = True\nlet invalid = if condition 1 else 2\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("legacy if syntax should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("argument 1 of macro `if` must be `Braced")
    }));
}

#[test]
fn diagnoses_duplicate_and_ambiguous_macro_overloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("identical overload patterns should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate macro overload `same: Expr`")
    }));

    let ambiguous = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro crossed = left: Ident String => right: Expr => parse_quote { 1 }\n",
                "macro crossed = left: Expr => right: Ident String => parse_quote { 2 }\n",
                "crossed first second\n",
            ),
            root,
        )
        .expect("ambiguous overload source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(ambiguous)
        .expect_err("incomparable overloads should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "ambiguous invocation of macro `crossed`" })
    );
}

#[test]
fn rejects_a_mismatched_literal_identifier_macro_argument() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro conditional = value: Expr => _: Ident \"else\" => parse_quote { $value }\n",
                "conditional 1 otherwise\n",
            ),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("literal identifier mismatch should fail expansion");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("argument 2 of macro `conditional` must be identifier `else`")
    }));
}

#[test]
fn rejects_bare_literal_identifier_macro_parameters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro conditional = value: Expr => Ident \"else\" => parse_quote { $value }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("bare literal identifier parameters should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("parameters must match atomic syntax values")
    }));
}

#[test]
fn subtype_bound_call_preserves_string_literal_type() {
    let module = type_check(concat!(
        "def string_identity: <T where T <: String> T -> T = x => x\n",
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
        "def string_identity: <T where T <: String> T -> T = x => x\n",
        "string_identity 1\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("`I32` does not satisfy `T <: String`");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "subtype bound is not satisfied: `I32` is not a subtype of `String`"
    }));
}

#[test]
fn subtype_bound_reflexivity() {
    type_check(concat!(
        "def echo: <T where T <: T> T -> T = x => x\n",
        "echo \"hello\"\n",
        "echo 1\n",
    ));
}

#[test]
fn subtype_bound_union_introduction() {
    type_check(concat!(
        "def widen: <T where T <: I32 | String> T -> () = x => ()\n",
        "widen 1\n",
        "widen \"text\"\n",
    ));
}

#[test]
fn subtype_bound_union_elimination() {
    type_check(concat!(
        "def accept: <T where T <: I32 | String | F64> T -> () = x => ()\n",
        "let value: I32 | String = 1\n",
        "accept value\n",
    ));
}

#[test]
fn ident_rejects_non_string_spelling_types() {
    let module = resolve(concat!(
        "type alias InvalidIdent = Ident I32\n",
        "let invalid: InvalidIdent\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("Ident's spelling argument must satisfy `Spelling <: String`");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "subtype bound is not satisfied: `I32` is not a subtype of `String`"
    }));
}

#[test]
fn default_type_bound_fills_omitted_type_argument() {
    let module = type_check(concat!(
        "type alias Box (T = String) = (value: T)\n",
        "let boxed: Box = (value: \"hi\")\n",
        "let explicit: Box I32 = (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("defaulted and explicit instantiations should compile");
}

#[test]
fn default_type_bound_rejects_mismatched_default_value() {
    let module = resolve(concat!(
        "type alias Box (T = String) = (value: T)\n",
        "let boxed: Box = (value: 42)\n",
    ));
    TypeChecker::new()
        .check(module)
        .expect_err("defaulted `Box` should require a `String` value, not `I32`");
}

#[test]
fn default_type_bound_may_reference_an_earlier_parameter() {
    let module = type_check(concat!(
        "type alias Pair A (B = A) = (A, B)\n",
        "let same: Pair I32 = (1, 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("chained default should compile");
}

#[test]
fn default_type_bound_referencing_an_earlier_parameter_rejects_mismatch() {
    let module = resolve(concat!(
        "type alias Pair A (B = A) = (A, B)\n",
        "let bad: Pair I32 = (1, \"x\")\n",
    ));
    TypeChecker::new()
        .check(module)
        .expect_err("`B` should default to `I32`, not accept a `String`");
}

#[test]
fn default_type_bound_is_checked_against_subtype_bound() {
    let module = resolve(concat!(
        "type alias Constrained (T = I32) where T <: String = T\n",
        "let bad: Constrained\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("`I32` default should not satisfy `T <: String`");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "subtype bound is not satisfied: `I32` is not a subtype of `String`"
    }));
}

#[test]
fn default_type_bound_does_not_fire_when_a_later_parameter_lacks_one() {
    let module = resolve(concat!(
        "type alias Weird (A = I32) B = (A, B)\n",
        "let bad: Weird = (1, 2)\n",
    ));
    let diagnostics = TypeChecker::new().check(module).expect_err(concat!(
        "`Weird`'s `B` parameter has no default, so the defaulted `A` ",
        "cannot fill in for a fully bare `Weird` either"
    ));
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "expected `Weird`, found `(I32, I32)`" })
    );
}

#[test]
fn trait_default_type_bound_fills_missing_implementation_and_bound_arguments() {
    let module = type_check(concat!(
        "trait Converts From (To = String) { convert: From -> To }\n",
        "impl Converts I32 { def convert = value => to_string value }\n",
        "def show: <T where Converts T> T -> String = value => convert value\n",
        "let text: String = show 42\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("defaulted trait argument should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn inline_default_type_bound_introduces_and_defaults_in_one_clause() {
    let module = type_check(concat!(
        "type alias Pair A (B = A) = (A, B)\n",
        "let same: Pair I32 = (1, 2)\n",
        "let overridden: Pair I32 String = (1, \"x\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("inline default should compile just like the two-clause form");
}

#[test]
fn inline_default_type_bound_combines_with_a_trailing_subtype_bound() {
    let module = type_check(concat!(
        "type alias Ident2 (Spelling = String) where Spelling <: String = Spelling\n",
        "let literal: Ident2 = \"answer\"\n",
        "let widened: Ident2 String = \"answer\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("inline default combined with a trailing subtype bound should compile");
}

#[test]
fn inline_default_type_bound_for_trait_parameter_fills_missing_argument() {
    let module = type_check(concat!(
        "trait Converts From (To = String) { convert: From -> To }\n",
        "impl Converts I32 { def convert = value => to_string value }\n",
        "def show: <T where Converts T> T -> String = value => convert value\n",
        "let text: String = show 42\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("inline trait default should compile just like the two-clause form");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn rejects_duplicate_default_type_bound_for_the_same_parameter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("type alias Bad (T = I32) (T = String) = T\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("`T` cannot have two conflicting defaults");
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
        .compile_module(&module)
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
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("excess arguments after a top-level `quote` expansion should still apply");
    assert!(
        llvm.contains("call <{}> %closure.code"),
        "expected the expanded `println` call to survive expansion:\n{llvm}",
    );
}

#[test]
fn diagnoses_incomplete_and_non_syntax_macros() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let incomplete = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro pair = left => right => parse_quote { ($left, $right) }\npair 1\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(incomplete)
        .expect_err("incomplete macro call should fail");
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
        .expect_err("non-Syntax macro result should fail without invocation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "macro `invalid` must return `Syntax`")
    );
}

#[test]
fn rejects_runtime_syntax_values() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let generated: Syntax\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("Syntax should not reach runtime checking");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`Syntax` values are compile-time-only")
    );
}

#[test]
fn diagnoses_recursive_macro_expansion() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("recursive expansion should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "recursive macro expansion of `recursive`")
    );
}

#[test]
fn rejects_repeated_splices_with_a_specific_diagnostic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("macro many = values => parse_quote { $values... }\nmany 1\n"),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("repeated splice should be reserved");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "repeated splices are not supported yet")
    );
}

#[test]
fn type_checks_function_declarations_without_values() {
    type_check("let add: (x: I32, y: I32) -> I32\n");
}

#[test]
fn reports_unknown_names_without_panicking() {
    let syntax = parse("missing\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("unknown name should fail resolution");

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "unknown name `missing`");
}

#[test]
fn reports_duplicate_definitions() {
    let syntax = parse("def value = 1\ndef value = 2\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("duplicate name should fail resolution");

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
        .expect_err("a pattern must not bind the same name twice");

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
        .expect_err("a `let` must not be shadowed by a `def`");
    assert_eq!(diagnostics[0].message, "duplicate definition of `x`");

    let syntax = parse("def x = () => x\nlet x = 1\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("a `def` must not be shadowed by a `let`");
    assert_eq!(diagnostics[0].message, "duplicate definition of `x`");
}

#[test]
fn rejects_two_pub_bindings_of_the_same_name_but_allows_pub_and_private_to_shadow() {
    let syntax = parse("pub let value = 1\npub let value = 2\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("two `pub` bindings of the same name must stay an error");
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
        .compile_module(&module)
        .expect("a shadowing `let` should compile");
    assert!(llvm.contains("add"));
}

#[test]
fn generates_a_named_function_after_predeclaring_it() {
    let module = type_check("let first = (a: I32, b: I32) => a\nfirst (1, 2)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("module should compile");

    assert!(llvm.contains("define i32 @first(ptr %0, i32 %a, i32 %b)"));
    assert!(llvm.contains("closure.call"));
}

#[test]
fn predeclares_functions_for_recursion() {
    let module = type_check("def recurse: (n: I32) -> I32 = (n: I32) => recurse (n)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("recursive closure should compile");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("load { i32 }"));
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
        .compile_module(&module)
        .expect("captured locals should use closure lowering");

    assert!(llvm.contains("@malloc"));
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("load { i32 }"));
}

#[test]
fn type_checks_and_lowers_managed_refs() {
    let module = type_check(concat!(
        "let point: Ref (x: I32, y: I32) = Ref (x: 1, y: 2)\n",
        "let copy = point\n",
        "let x: I32 = copy.x\n",
        "let Ref (captured_x, captured_y) = point\n",
        "def sum_ref: (Ref (x: I32, y: I32)) -> I32 = value => match value {\n",
        "  Ref (x, y) => x + y,\n",
        "}\n",
        "let nested: Ref (Ref I32) = Ref (Ref 9)\n",
        "let empty: Ref () = Ref ()\n",
        "let total: I32 = captured_x + captured_y\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Ref values should generate valid LLVM");
    assert!(llvm.contains("call ptr @__staple_gc_alloc"));
    assert!(llvm.contains("ref.payload"));
    assert!(llvm.contains("@__staple_gc_collect"));
    assert!(llvm.contains("call void @__staple_gc_register_root"));
}

#[test]
fn preserves_literal_nominal_ref_container_semantics() {
    let module = type_check(concat!(
        "type RefPoint = Ref (x: I32, y: I32)\n",
        "let point: RefPoint = RefPoint (Ref (x: 3, y: 4))\n",
        "let x: I32 = point.x\n",
        "let RefPoint (Ref (captured_x, captured_y)) = point\n",
        "let y: I32 = captured_y\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nominal Ref containers should compile");
}

#[test]
fn rejects_incomplete_and_overapplied_ref_types() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let value: Ref\n"))
        .expect_err("Ref requires a payload type");
    assert!(!diagnostics.is_empty());

    let diagnostics = TypeChecker::new()
        .check(resolve("let value: Ref I32 I32\n"))
        .expect_err("Ref accepts exactly one payload type");
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
            &stapler::CheckedFunctionType {
                parameter: Box::new(CheckedType::I32),
                mutations: Vec::new(),
                effects: stapler::CheckedEffectSet::default(),
                result: Box::new(CheckedType::Function(stapler::CheckedFunctionType {
                    parameter: Box::new(CheckedType::I32),
                    mutations: Vec::new(),
                    effects: stapler::CheckedEffectSet::default(),
                    result: Box::new(CheckedType::I32),
                })),
            },
        );
    }

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("curried functions should compile");
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("@malloc"));
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
        .expect_err("an untyped parameter needs context");
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
        .compile_module(&module)
        .expect("transitive captures should compile");
    assert!(llvm.matches("@malloc").count() >= 3);
    assert!(llvm.contains("load { i32 }"));
    assert!(llvm.contains("load { i32, i32 }"));
}

#[test]
fn adapts_non_variadic_externs_used_as_function_values() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { let puts: (CPointer CChar) -> I32 }\n",
        "def apply: ((CPointer CChar) -> I32, CPointer CChar) -> I32 = (f, value) => f value\n",
        "apply (puts, c_string \"hello\")\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("external function adapter should compile");
    assert!(llvm.contains("define internal i32 @__staple_extern_puts"));
    assert!(llvm.contains("call i32 @puts"));
}

#[test]
fn compiles_builtin_arithmetic_via_trait_dispatch() {
    let module = type_check("let answer: I32 = 1 + 2\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("builtin `+` should compile via ordinary trait dispatch");
    // "operator.2b" is the mangled symbol a binding literally named `+` would
    // get; since `+` is fixed grammar now, not a binding, it must not appear.
    assert!(!llvm.contains("@operator.2b"));
    assert!(llvm.contains("add"));
}

#[test]
fn decodes_source_string_literals_before_llvm_generation() {
    let source = concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { let puts: (CPointer CChar) -> I32 }\n",
        "puts (c_string \"hello\\n\")\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("module should compile");

    assert!(llvm.contains("c\"hello\\0A\\00\""));
    assert!(!llvm.contains("\\22hello"));
}

#[test]
fn type_checks_generic_aliases_and_functions() {
    let module = type_check(concat!(
        "type alias Pair (A, B) = (A, B)\n",
        "def identity: <T> T -> T = x => x\n",
        "let pair: Pair (String, I32) = (\"answer\", 42)\n",
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
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic functions should be monomorphized");
    assert!(llvm.matches("identity__").count() >= 2);
}

#[test]
fn type_checks_static_traits_and_bounded_generic_functions() {
    let module = type_check(concat!(
        "trait Increment T { increment: T -> T }\n",
        "trait Echo T { echo: T -> T }\n",
        "trait Swap T { swap: T -> T }\n",
        "impl Increment I32 { def increment = value => value + 1 }\n",
        "impl Echo I32 { def echo = value => value }\n",
        "impl Swap (I32, I32) { def swap = (left, right) => (right, left) }\n",
        "def increment_twice: <T where Increment T> T -> T = value => increment (increment value)\n",
        "def increment_echo: <T where Increment T, Echo T> T -> T = value => echo (increment value)\n",
        "let direct: I32 = Increment.increment 40\n",
        "let answer: I32 = increment_twice direct\n",
        "let bounded: I32 = increment_echo answer\n",
        "let first_class: I32 -> I32 = Increment.increment\n",
        "let other: I32 = first_class bounded\n",
        "let swapped: (I32, I32) = Swap.swap (1, 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("static trait calls should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn provides_to_string_for_prelude_scalar_types() {
    let module = type_check(concat!(
        "def render: <T where ToString T> T -> String = value => to_string value\n",
        "let a = render (1 satisfies I8)\nlet b = render (1 satisfies I16)\n",
        "let c = render (1 satisfies I32)\nlet d = render (1 satisfies I64)\n",
        "let e = render (1 satisfies U8)\nlet f = render (1 satisfies U16)\n",
        "let g = render (1 satisfies U32)\nlet h = render (1 satisfies U64)\n",
        "let i = render (1 satisfies ISize)\nlet j = render (1 satisfies USize)\n",
        "let k = render (1.5 satisfies F32)\nlet l = render (1.5 satisfies F64)\n",
        "let boolean: Bool = True\nlet m = render boolean\n",
        "let string: String = \"text\"\nlet n = render string\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("ToString implementations should generate LLVM");
}

#[test]
fn provides_formatter_display_debug_and_structural_product_debug() {
    let module = type_check(concat!(
        "let mut formatter = Formatter.new ()\n",
        "Formatter.write (formatter, \"left\")\n",
        "Formatter.write (formatter, \" + right\")\n",
        "let written: String = Formatter.finish formatter\n",
        "let displayed: String = Formatter.display 42\n",
        "let escaped: String = Formatter.debug \"hello\\n\\\"world\"\n",
        "let product: (I32, name: String, Bool) = (1, name: \"two\", True)\n",
        "let product_debug: String = Formatter.debug product\n",
        "let empty_debug: String = Formatter.debug ()\n",
        "type Point = (x: I32, y: I32)\n",
        "impl Debug Point {\n",
        "  def fmt = (Point (x, y), formatter) => {\n",
        "    Formatter.write (formatter, \"Point \" )\n",
        "    Debug.fmt ((x: x, y: y), formatter)\n",
        "  }\n",
        "}\n",
        "let point_debug: String = Formatter.debug (Point (x: 3, y: 4))\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("formatting protocols should generate LLVM");
    assert!(llvm.contains("__staple_structural_Debug"));
    assert!(llvm.contains("formatter.write"));
}

#[test]
fn provides_structural_debug_for_sum_types() {
    let module = type_check(concat!(
        "let integer: I32 | String = 42\n",
        "let string: I32 | String = \"text\"\n",
        "let integer_debug: String = Formatter.debug integer\n",
        "let string_debug: String = Formatter.debug string\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("sum Debug should generate LLVM");
    assert!(llvm.contains("__staple_structural_Debug"));
    assert!(llvm.contains("debug.sum.fmt"));
}

#[test]
fn derives_debug_for_nominal_representations() {
    let module = type_check(concat!(
        "@derive_debug\ntype Point = (x: I32, y: I32)\n",
        "@derive_debug\ntype Choice = I32 | String\n",
        "@derive_debug\ntype Box T = T\n",
        "let point_debug: String = Formatter.debug (Point (x: 3, y: 4))\n",
        "let choice_debug: String = Formatter.debug (Choice (42 satisfies I32 | String))\n",
        "let box_debug: String = Formatter.debug (Box 7)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("derived Debug implementations should generate LLVM");
    assert!(llvm.contains("formatter.write"));
    assert!(llvm.contains("__staple_structural_Debug"));
}

#[test]
fn exposes_type_declarations_as_structured_items() {
    let module = type_check(concat!(
        "macro @inspect_item: Item -> Item = item => match item {\n",
        "  TypeDeclarationItem (DistinctDeclaration(), Ident name, spelling, declared_type, parameters, Some representation) => item,\n",
        "  UnstructuredItem() => item,\n}\n",
        "@inspect_item\ntype Pair T = (T, T)\n",
        "@inspect_item\ndef answer = () => 42\n",
        "let pair = Pair (1, 2)\nlet result = answer ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("structured and fallback item views should round-trip");
}

#[test]
fn type_checks_and_generates_string_templates() {
    let module = type_check(concat!(
        "type Label = String\n",
        "impl Display Label {\n",
        "  def fmt = (Label value, formatter) => Formatter.write (formatter, value)\n",
        "}\n",
        "def render: <T where Display T> T -> String = value => \"value=$value\"\n",
        "let name: String = \"world\"\n",
        "let answer: I32 = 42\n",
        "let product = (answer, name)\n",
        "let message: String = \"hello $name: ${answer}; ${product:?}; \\$5\"\n",
        "let generic: String = render answer\n",
        "let nominal: String = \"label=${Label \"tag\"}\"\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("string templates should generate LLVM");
    assert!(llvm.contains("template.formatter"));
    assert!(llvm.contains("template.fmt"));
    assert!(llvm.contains("template.finish"));
}

#[test]
fn string_templates_require_the_selected_formatting_trait() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Secret = I32\n",
            "let secret = Secret 1\n",
            "let message = \"$secret\"\n",
        )))
        .expect_err("Display is required for ordinary interpolation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("trait bound is not satisfied") })
    );
}

#[test]
fn structural_debug_requires_debug_elements_and_does_not_expose_nominal_representations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Secret = I32\n",
            "let secret = Secret 1\n",
            "let product = (secret,)\n",
            "let text = Formatter.debug product\n",
        )))
        .expect_err("a product element without Debug must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("trait bound is not satisfied") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Secret = I32\n",
            "let secret = Secret 1\n",
            "let text = Formatter.debug secret\n",
        )))
        .expect_err("a nominal type must not inherit its representation's Debug implementation");
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
        "impl Increment I32 { def increment = value => value + 1 }\n",
        "let direct: I32 = Increment.twice 40\n",
        "let first_class: I32 -> I32 = Increment.twice\n",
        "let answer: I32 = first_class direct\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("default trait members should specialize and compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn default_trait_members_use_prerequisites_multiple_arguments_and_macros() {
    let module = type_check(concat!(
        "trait Same T where Eq T { same: (T, T) -> Bool = (left, right) => Eq.equal left right }\n",
        "trait Select Value { select: (Bool, Value, Value) -> Value = (condition, left, right) => if { condition => left, else => right } }\n",
        "trait First (Left, Right) { first: (Left, Right) -> Left = (left, right) => left }\n",
        "impl Same I32 {}\n",
        "impl Select I32 {}\n",
        "impl First (I32, String) {}\n",
        "let condition: Bool = Same.same (42, 42)\n",
        "let selected: I32 = Select.select (condition, 1, 2)\n",
        "let first: I32 = First.first (selected, \"ignored\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("prerequisite and macro-using defaults should compile");
}

#[test]
fn explicit_trait_members_override_defaults() {
    let module = type_check(concat!(
        "trait Identity T { identity: T -> T = value => value }\n",
        "impl Identity I32 { def identity = value => value + 1 }\n",
        "let answer: I32 = Identity.identity 41\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("recursive defaults should reuse their specialization");
}

#[test]
fn rejects_invalid_default_trait_member_bodies() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "trait Invalid T { identity: T -> T = value => \"wrong\" }\n",
        ))
        .expect_err("default bodies must match their member type");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("expected `T`, found `String`") })
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("trait Invalid T { identity: T -> T = 42 }\n", root)
        .expect("invalid default source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("default bodies must be function values");
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
        "impl Merge I32 I32 I32 { def merge = (left, right) => left + right }\n",
        "impl Convert (I32, String) { def convert = value => \"converted\" }\n",
        "def combine: <L, R, O where Merge L R O> (L, R) -> O = pair => Merge.merge pair\n",
        "let total: I32 = combine (20, 22)\n",
        "let converted: String = Convert.convert total\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("multi-parameter trait calls should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn infers_trait_functional_dependency_arguments() {
    let module = type_check(concat!(
        "trait Iterator Iter Item where Iter ~> Item { next: Iter -> Item }\n",
        "impl Iterator I32 String { def next = value => \"next\" }\n",
        "trait AddTo Left Right Output where {Left, Right} ~> Output { add_to: Left -> Right -> Output }\n",
        "impl AddTo I32 I32 I32 { def add_to = left => right => left + right }\n",
        "trait Chain A B C where A ~> B, B ~> C { chained: A -> (B, C) }\n",
        "impl Chain I32 String U8 { def chained = value => (\"chain\", 7) }\n",
        "trait ConvertPair (From, To) where From ~> To { convert_pair: From -> To }\n",
        "impl ConvertPair (I32, String) { def convert_pair = value => \"pair\" }\n",
        "def requires_iterator: <T where Iterator T> T -> () = value => ()\n",
        "def requires_iterator_explicit: <T where Iterator T _> T -> () = value => ()\n",
        "def requires_add: <T where AddTo T T> T -> T = value => value\n",
        "def requires_pair: <T where ConvertPair (T, _)> T -> () = value => ()\n",
        "trait UsesIterator Iter where Iterator Iter { use_iterator: Iter -> Iter }\n",
        "impl UsesIterator I32 { def use_iterator = value => value }\n",
        "let next_value = Iterator.next 1\n",
        "let next_string: String = next_value\n",
        "let sum: I32 = AddTo.add_to 20 22\n",
        "let chained = Chain.chained 1\n",
        "let _: () = requires_iterator 1\n",
        "let _: () = requires_iterator_explicit 1\n",
        "let _: I32 = requires_add 1\n",
        "let _: () = requires_pair 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("functional dependency dispatch should compile");
}

#[test]
fn rejects_invalid_functional_dependency_uses_and_conflicting_impls() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert From To where From ~> To { convert: From -> To }\n",
            "impl Convert I32 String { def convert = value => \"one\" }\n",
            "impl Convert I32 I32 { def convert = value => value }\n",
        )))
        .expect_err("functional dependencies must make implementations coherent");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("violate a functional dependency")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait AddTo Left Right Output where {Left, Right} ~> Output { add_to: Left -> Right -> Output }\n",
            "def invalid: <T where AddTo T _ T> T -> T = value => value\n",
        )))
        .expect_err("non-dependent arguments cannot be inferred");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot be inferred from functional dependencies")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert From To where From ~> To { convert: From -> To }\n",
            "impl Convert I32 { def convert = value => value }\n",
        )))
        .expect_err("implementation headers remain exact-arity");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expects 2 compile-time arguments, found 1")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Iterator Iter Item where Iter ~> Item { next: Iter -> Item }\n",
            "def invalid: <Iter, Item where Iterator Iter Item, Iterator Iter String> Iter -> Iter = value => value\n",
        )))
        .expect_err("active bounds must respect functional dependencies");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("trait bounds conflict with a functional dependency")
    }));
}

#[test]
fn rejects_invalid_functional_dependency_declarations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "trait Invalid A B where Missing ~> B { convert: A -> B }\n",
            "unknown trait type parameter `Missing` in functional dependency",
        ),
        (
            "trait Invalid A B where {A, A} ~> B { convert: A -> B }\n",
            "duplicate functional dependency determinant `A`",
        ),
        (
            "trait Invalid A where A ~> A { convert: A -> A }\n",
            "functional dependency cannot determine one of its determinants",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .expect("invalid dependency source should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid functional dependency must not resolve");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected))
        );
    }
}

#[test]
fn preserves_applied_types_as_unary_trait_arguments() {
    let module = type_check(concat!(
        "type Box T = (value: T)\n",
        "trait Echo T { echo: T -> T }\n",
        "impl Echo Box I32 { def echo = value => value }\n",
        "def echo_box: <T where Echo Box T> (Box T) -> Box T = value => Echo.echo value\n",
        "let boxed: Box I32 = Box 42\n",
        "let echoed: Box I32 = echo_box boxed\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("legacy unary applied trait arguments should compile");
}

#[test]
fn enforces_and_propagates_transitive_trait_prerequisites() {
    let module = type_check(concat!(
        "trait Base T { base: T -> T }\n",
        "trait Middle T where Base T { middle: T -> T }\n",
        "trait Derived T where Middle T { derived: T -> T }\n",
        "impl Derived I32 { def derived = value => value }\n",
        "impl Middle I32 { def middle = value => value }\n",
        "impl Base I32 { def base = value => value }\n",
        "def apply: <T where Derived T> T -> T = value => Base.base (Middle.middle (Derived.derived value))\n",
        "let answer: I32 = apply 42\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("transitive prerequisite dispatch should compile");
    assert!(llvm.contains("trait.call"));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Base T { base: T -> T }\n",
            "trait Derived T where Base T { derived: T -> T }\n",
            "impl Derived I32 { def derived = value => value }\n",
        )))
        .expect_err("implementations must satisfy trait prerequisites");
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
        "impl Duplicate I32 { def duplicate = value => value }\n",
        "def pair: <T where Duplicate T> T -> (T, T) = value => (value, value)\n",
        "let values: (I32, I32) = pair 42\n",
    ));
}

#[test]
fn substitutes_product_parameters_into_multiple_prerequisites() {
    type_check(concat!(
        "trait BothEqual (Left, Right) where Eq Left, Eq Right { equal: (Left, Left, Right, Right) -> (Bool, Bool) }\n",
        "impl BothEqual (I32, I32) { def equal = (left_a, left_b, right_a, right_b) => (Eq.equal left_a left_b, Eq.equal right_a right_b) }\n",
        "def compare_both: <Left, Right where BothEqual (Left, Right)> (Left, Left, Right, Right) -> (Bool, Bool) = (left_a, left_b, right_a, right_b) => (Eq.equal left_a left_b, Eq.equal right_a right_b)\n",
        "let result: (Bool, Bool) = compare_both (1, 1, 2, 2)\n",
    ));
}

#[test]
fn rejects_cyclic_trait_prerequisites() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
        .expect_err("prerequisite cycles must be rejected");
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
            "impl Add I32 I32 { def add = pair => 0 }\n",
        )))
        .expect_err("trait implementation arity must match the declaration");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expects 3 compile-time arguments, found 2")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "trait Invalid Left Right { keep_left: Left -> Left }\n",
        ))
        .expect_err("every member must mention every trait parameter");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("must mention trait parameter `Right`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert (From, To) { convert: From -> To }\n",
            "impl Convert I32 { def convert = value => value }\n",
        )))
        .expect_err("product binders require product arguments");
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
        .expect_err("implementations must be complete");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("missing member `increment`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "type alias Number = I32\n",
            "impl Increment I32 { def increment = value => value }\n",
            "impl Increment Number { def increment = value => value }\n",
        )))
        .expect_err("aliases may not create overlapping implementations");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment I32 { def increment = value => \"wrong\" }\n",
        )))
        .expect_err("implementation bodies must match their trait member types");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected `I32`, found `String`")),
        "{diagnostics:?}"
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment I32 { def increment = value => value }\n",
            "def invalid: <T> T -> T = value => increment value\n",
        )))
        .expect_err("generic callers must propagate trait bounds");
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
        .expect_err("trait members must be functions that mention the parameter");
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
            "impl Increment _ { def increment = value => value }\n",
        )))
        .expect_err("implementation targets must be concrete");
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
        "impl Bound I32 { def check = value => True }\n",
        "impl <T where Bound T> Target T { def act = value => Bound.check value }\n",
        "let answer: Bool = Target.act 41\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a conditional trait implementation should monomorphize and compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn rejects_generic_trait_implementation_dispatch_when_bound_is_unmet() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl <T where Bound T> Target T { def act = value => value }\n",
            "let answer: I32 = Target.act 41\n",
        )))
        .expect_err("I32 does not implement Bound, so the conditional impl must not apply");
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
            "impl <T where Bound T> Target T { def act = value => value }\n",
            "impl <U where Bound U> Target U { def act = value => value }\n",
        )))
        .expect_err("alpha-equivalent generic implementations must be rejected as duplicates");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));
}

#[test]
fn rejects_ambiguous_dispatch_between_blanket_and_concrete_trait_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl Bound I32 { def check = value => True }\n",
            "impl Target I32 { def act = value => value }\n",
            "impl <T where Bound T> Target T { def act = value => value }\n",
            "let answer: I32 = Target.act 41\n",
        )))
        .expect_err("a concrete impl and an applicable blanket impl must be ambiguous");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("ambiguous trait implementation")
    }));
}

#[test]
fn cyclic_trait_implementation_bound_fails_without_hanging() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Cyclic T { check: T -> Bool }\n",
            "impl <T where Cyclic T> Cyclic T { def check = value => True }\n",
            "let answer: Bool = Cyclic.check 41\n",
        )))
        .expect_err("a self-referential bound must fail rather than being accepted coinductively");
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
        "impl Left I32 { def convert = value => value }\n",
        "impl Right I32 { def convert = value => value }\n",
        "let left: I32 = Left.convert 1\n",
        "let right: I32 = Right.convert 2\n",
        "let ambiguous: I32 = convert 3\n",
    );
    let diagnostics = TypeChecker::new()
        .check(resolve(source))
        .expect_err("unqualified overlapping method names must be ambiguous");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("ambiguous trait method; qualify the trait name")
    }));
}

#[test]
fn constructs_distinct_and_generic_distinct_values() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "type Box T = (value: T)\n",
        "let user: UserId = UserId 42\n",
        "let boxed: Box I32 = Box (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
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
        "def select = (value: Foo | Bar) => { let Foo()? = value; Foo }\n",
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
        .compile_module(&module)
        .expect("singleton values should generate valid LLVM");
}

#[test]
fn opaque_types_do_not_introduce_values_and_singletons_are_not_callable() {
    let diagnostics = NameResolver::new()
        .resolve(&parse("pub type Secret = opaque\nSecret\n").expect("source should parse"))
        .expect_err("opaque type should not introduce a value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown name `Secret`")
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("pub type Foo;\nFoo()\n"))
        .expect_err("singleton value should not be callable");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot call a value of type `Foo`")
    }));
}

#[test]
fn contextually_specializes_first_class_generic_functions() {
    let module = type_check(concat!(
        "def identity: <T> T -> T = x => x\n",
        "def apply: (I32 -> I32) -> I32 = f => f 42\n",
        "let int_identity: I32 -> I32 = identity\n",
        "let answer: I32 = apply identity\n",
        "let other: I32 = int_identity 7\n",
        "def apply_pair: (I32 -> I32, I32) -> I32 = (f, x) => f x\n",
        "let paired: I32 = apply_pair (identity, 9)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("contextually specialized generic functions should compile");
}

#[test]
fn contextually_specializes_first_class_constructors() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "type Box T = (value: T)\n",
        "let make_user: I32 -> UserId = UserId\n",
        "let make_box: (value: I32) -> Box I32 = Box\n",
        "let user: UserId = make_user 42\n",
        "let boxed: Box I32 = make_box (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("first-class constructors should compile");
}

#[test]
fn destructures_nominal_values_in_lets_and_functions() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "type PairIds = (UserId, UserId)\n",
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
        .compile_module(&module)
        .expect("nominal destructuring should be zero-cost");
}

#[test]
fn accesses_visible_nominal_representations_explicitly_and_by_shortcut() {
    let module = type_check(concat!(
        "type User = (name: String, age: I32)\n",
        "type Inner = (name: String, tag: I32)\n",
        "type Outer = Inner\n",
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
        .compile_module(&module)
        .expect("representation projections should be zero-cost and preserve places");
}

#[test]
fn representation_access_requires_a_nominal_value_and_unwraps_one_shortcut_layer() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Inner = (name: String, tag: I32)\n",
            "type Outer = Inner\n",
            "let outer = Outer (Inner (name: \"Ada\", tag: 1))\n",
            "let invalid: String = outer.name\n",
            "let also_invalid = (42).*\n",
        )))
        .expect_err("invalid representation projections should be rejected");
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
        "type Box T = (value: T)\n",
        "def unbox: <T> Box T -> T = Box (value) => value\n",
        "let answer: I32 = unbox (Box (value: 42))\n",
        "let text: String = unbox (Box (value: \"hello\"))\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic nominal patterns should monomorphize");
}

#[test]
fn captures_values_bound_by_nominal_patterns() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "def capture: UserId -> (() -> I32) = user => {\n",
        "  let UserId id = user\n",
        "  () => id\n",
        "}\n",
        "let get_id: () -> I32 = capture (UserId 42)\n",
        "let answer: I32 = get_id ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("destructured leaves should be captured normally");
}

#[test]
fn rejects_non_nominal_and_mismatched_nominal_patterns() {
    let syntax = parse(concat!(
        "type alias Number = I32\n",
        "let Number value = 42\n",
    ))
    .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("aliases cannot be used as nominal patterns");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("not a represented nominal type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Left = I32\n",
            "type Right = I32\n",
            "let right: Right = Right 42\n",
            "let Left value = right\n",
        )))
        .expect_err("a nominal pattern must match the same nominal type");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("cannot match"))
    );
}

#[test]
fn monomorphizes_nested_and_recursive_generic_calls() {
    let module = type_check(concat!(
        "def identity: <T> T -> T = x => x\n",
        "def copy: <U> U -> U = x => identity x\n",
        "def recur: <V> V -> V = x => recur x\n",
        "let answer: I32 = copy 42\n",
        "let recurse: I32 = recur 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested generic calls should discover concrete specializations");
}

#[test]
fn monomorphizes_generic_closures_with_captures() {
    let module = type_check(concat!(
        "def outer: I32 -> I32 = y => {\n",
        "  def inner: <T> T -> I32 = x => y\n",
        "  inner \"ignored\"\n",
        "}\n",
        "let answer: I32 = outer 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic closures should retain their captured environment");
}

#[test]
fn infers_product_and_result_only_compile_time_parameters() {
    let module = type_check(concat!(
        "def first: <A, B> (A, B) -> A = (a, b) => a\n",
        "type Phantom T = I32\n",
        "def make: <T> I32 -> Phantom T = x => Phantom x\n",
        "let answer: I32 = first (42, \"ignored\")\n",
        "let contextual: Phantom String = make 7\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("multiple compile-time parameters should specialize");
}

#[test]
fn monomorphizes_curried_generic_function_layers() {
    let module = type_check(concat!(
        "def keep_first: <A, B where Copy A> A -> B -> A = a => b => a\n",
        "let answer: I32 = keep_first 42 \"ignored\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("curried generic layers should specialize together");
}

#[test]
fn allows_generic_locals_bound_by_the_enclosing_def() {
    let module = type_check(concat!(
        "def make_list: <T where Default T> () -> T[32] = () => {\n",
        "  let list: T[32] = default ()\n",
        "  list\n",
        "}\n",
        "let ints: I32[32] = make_list ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a local bound by the enclosing generic def should specialize");
}

#[test]
fn allows_generic_locals_inside_a_nested_closure() {
    let module = type_check(concat!(
        "def make_pair: <T where Default T> () -> (T, T) = () => {\n",
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
        .compile_module(&module)
        .expect("a nested closure should inherit the enclosing generic scope");
}

#[test]
fn rejects_unconstrained_generic_values_and_non_function_schemes() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def identity: <T> T -> T = x => x\n",
            "def copied = identity\n",
            "def invalid: <U> U = 42\n",
        )))
        .expect_err("generic values require a concrete use or function scheme");
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        messages
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
        .check(resolve("def grow: <T> T -> T = x => grow (x, x)\n"))
        .expect_err("recursive calls may not change their specialization");
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
        .compile_module(&module)
        .expect("nested return should generate valid LLVM");
    assert!(llvm.contains("ret i32 42"));
}

#[test]
fn rejects_returns_outside_functions_and_incompatible_return_values() {
    let outside = parse("return 1\n").expect("return item should parse");
    let diagnostics = NameResolver::new()
        .resolve(&outside)
        .expect_err("top-level return should not resolve");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("only allowed inside a function")
    }));

    let module = resolve("def invalid = () => { return 42; } satisfies String\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("return value should match the function result");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `String`, found `I32`")
    }));
}

#[test]
fn supports_contextual_literals_and_arithmetic_for_all_integer_types() {
    let source = concat!(
        "def i8_value = () => { let a: I8 = 8; let b: I8 = 4; (a + b) * b - a / b; } satisfies I8\n",
        "def i16_value = () => { let a: I16 = 8; let b: I16 = 4; (a + b) * b - a / b; } satisfies I16\n",
        "def i32_value = () => { let a: I32 = 8; let b: I32 = 4; (a + b) * b - a / b; } satisfies I32\n",
        "def i64_value = () => { let a: I64 = 8; let b: I64 = 4; (a + b) * b - a / b; } satisfies I64\n",
        "def u8_value = () => { let a: U8 = 8; let b: U8 = 4; (a + b) * b - a / b; } satisfies U8\n",
        "def u16_value = () => { let a: U16 = 8; let b: U16 = 4; (a + b) * b - a / b; } satisfies U16\n",
        "def u32_value = () => { let a: U32 = 8; let b: U32 = 4; (a + b) * b - a / b; } satisfies U32\n",
        "def u64_value = () => { let a: U64 = 8; let b: U64 = 4; (a + b) * b - a / b; } satisfies U64\n",
        "def isize_value = () => { let a: ISize = 8; let b: ISize = 4; (a + b) * b - a / b; } satisfies ISize\n",
        "def usize_value = () => { let a: USize = 8; let b: USize = 4; (a + b) * b - a / b; } satisfies USize\n",
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
        .compile_module(&module)
        .expect("all integer arithmetic should generate valid LLVM");
}

#[test]
fn rejects_out_of_range_and_mixed_integer_arithmetic() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let too_large: U8 = 256\n"))
        .expect_err("an out-of-range literal should fail");
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
        .expect_err("mixed integer arithmetic should require an explicit conversion");
    assert!(!diagnostics.is_empty());
}

#[test]
fn emits_a_native_object_file() {
    let module = type_check(include_str!("../examples/hello_world.sta"));
    let context = Context::create();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow the Unix epoch")
        .as_nanos();
    let output = std::env::temp_dir().join(format!("stapler-test-{nonce}.o"));

    CodeGenerator::new(&context)
        .emit_object(&module, &output, None)
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
        "type Point = (I32, I32)\n",
        "def copied = () => { let point = Point (1, 2); let other = point; point; other }\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "def invalid = (value: CString) => { let moved = value; value }\n",
        )))
        .expect_err("CString must be move-only");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use of moved value"))
    );
}

#[test]
fn exposes_copy_but_rejects_explicit_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Value = I32\n",
            "impl Copy Value {}\n",
        )))
        .expect_err("Copy implementations must be inferred");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`Copy` is implemented structurally")
    }));
}

#[test]
fn lowers_custom_drop_and_gc_finalizer_glue() {
    let module = type_check(concat!(
        "type Resource = I32\n",
        "impl Drop Resource { def drop = Resource value => () }\n",
        "def release = () => { let resource = Resource 7; drop resource }\n",
        "def managed = () => Ref (Resource 9)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Drop glue should compile");

    assert!(llvm.contains("drop.call"));
    assert!(llvm.contains("__staple_gc_set_finalizer"));
    assert!(llvm.contains("__staple_gc_finalize_"));
}

#[test]
fn rejects_move_only_globals_partial_moves_and_cstring_returns_from_c() {
    let ffi_diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { let invalid_return: () -> CString }\n",
        )))
        .expect_err("C must not manufacture owned CStrings");
    assert!(
        ffi_diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("cannot return owned `CString`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop.*\n",
            "let global: CString = c_string \"owned\"\n",
            "def partial = (pair: (CString, I32)) => pair.0\n",
        )))
        .expect_err("unsupported ownership operations should be rejected");
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("move-only values cannot be stored"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("cannot move a field"))
    );
}

#[test]
fn moves_resources_into_managed_closures_and_borrows_ref_payloads() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { let inspect: CString -> I32 }\n",
        "def make = (value: CString) => { let callback = () => inspect value; callback }\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a move-only capture should get managed finalizer glue");
    assert!(llvm.contains("__staple_gc_finalize_closure_"));

    for source in [
        concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { let inspect: CString -> I32 }\n",
            "def invalid = (value: CString) => { let callback = () => inspect value; value }\n",
        ),
        concat!(
            "use std.cinterop.*\n",
            "def invalid = (value: Ref CString) => { let Ref inner = value; drop inner }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("captured and Ref-owned resources cannot be moved again");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("moved value")
                || diagnostic.message.contains("cannot move this value")
        }));
    }
}

#[test]
fn lowers_path_sensitive_drop_flags() {
    let module = type_check(concat!(
        "type Resource = I32\n",
        "impl Drop Resource { def drop = Resource value => () }\n",
        "def conditional = (flag: Bool, resource: Resource) => match flag {\n",
        "  True() => { drop resource; () },\n",
        "  False() => (),\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("conditional ownership should lower through a runtime drop flag");
    assert!(llvm.contains("drop.is_live"));
    assert!(llvm.contains("drop.call"));
}

#[test]
fn supports_contextual_string_literal_types_and_widening() {
    let module = type_check(concat!(
        "type alias Answer = \"yes\" | \"no\"\n",
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
            .expect_err("invalid literal refinement should be rejected");
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
        "type Some = String\n",
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
        .compile_module(&module)
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
        .expect_err("a literal alternative is missing");
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
        .expect_err("a duplicate literal arm is unreachable");
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
        .expect_err("literal function patterns must be irrefutable");
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
        .compile_module(&module)
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
        .compile_module(&module)
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
            .expect_err("loop control should fail resolution");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing `{expected}` in {diagnostics:?}",
        );
    }

    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid: () -> I32 = () => loop { break }\n"))
        .expect_err("unit break should not satisfy I32");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("expected `I32`") && diagnostic.message.contains("found `()`")
    }));
}

#[test]
fn accepts_divergent_loops_in_typed_contexts() {
    let module = type_check("def forever: () -> I32 = () => loop { continue }\n");
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a divergent loop should satisfy its expected type");
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
        .compile_module(&module)
        .expect("code after a top-level loop with a break should still be checked and generated");
    assert!(llvm.contains("loop.body"));
}

#[test]
fn checks_ownership_across_loop_exits_and_back_edges() {
    let module = type_check(concat!(
        "type Resource = I32\n",
        "impl Drop Resource { def drop = Resource _ => () }\n",
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
        .compile_module(&module)
        .expect("loop exits should preserve moved results and drop iteration locals");
    assert!(llvm.contains("loop.value"));
    assert!(llvm.contains("drop.call"));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Resource = I32\n",
            "impl Drop Resource { def drop = Resource _ => () }\n",
            "def invalid = (value: Resource) => loop {\n",
            "  let consumed = value\n",
            "  continue\n",
            "}\n",
        )))
        .expect_err("an outer move-only value cannot be consumed before a back-edge");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("may be moved before the next loop iteration")
    }));
}

#[test]
fn pub_repr_type_constructor_satisfies_its_own_subtype_bound() {
    let module = type_check(concat!(
        "pub(repr) type Ident2 Spelling where Spelling <: String = Spelling\n",
        "let literal: Ident2 String = Ident2 \"answer\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("distinct type whose representation is a bare bounded parameter should compile");
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
        .compile_module(&module)
        .expect("`&&`/`||` should compile to short-circuiting branches");
    assert!(llvm.contains("logical.short_circuit"));
    assert!(llvm.contains("logical.right"));
    assert!(llvm.contains("logical.merge"));
}

#[test]
fn rejects_non_bool_logical_operands() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def bad: (I32, I32) -> I32 = (a, b) => a && b\n"))
        .expect_err("`&&` requires `Bool` operands, and is not overloadable for `I32`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("True | False"))
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
        .compile_module(&module)
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
        .compile_module(&module)
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
        .compile_module(&module)
        .expect("float const bindings should fold and compile");
    // `x` and `y` fold directly to literal constants. `z`'s negative
    // result instead goes through the same zero-minus-magnitude
    // desugaring already used for negative integer consts, so it's
    // computed by a call at module-init time rather than appearing as a
    // bare `double -2` literal; compiling successfully is enough to
    // exercise that path.
    assert!(llvm.contains("double 4.000000e+00"));
    assert!(llvm.contains("double 2.500000e+00"));
}

#[test]
fn rejects_const_float_initializers_that_fold_to_non_finite_values() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("const x: F64 = 1.0 / 0.0\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("dividing by zero at compile time should not fold to an infinite constant");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("is not finite"))
    );
}

#[test]
fn rejects_const_initializers_that_cannot_be_folded_to_a_compile_time_constant() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("const f = () => 1\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("a function value cannot be a compile-time constant");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot be represented as a compile-time constant")
    }));
}

#[test]
fn rejects_self_referential_const_bindings() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("const x = x + 1\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("a self-referential const should be rejected, not hang or crash");
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
    let diagnostics = TypeChecker::new()
        .check(resolved)
        .expect_err("reading a local binding before its initializer still fails to type-check");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("later"))
    );
}
