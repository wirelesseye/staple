// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker, TypedModule,
};
use staple_syntax::{Diagnostic, Span};
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn check(source: &str) -> Result<TypedModule, Vec<Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let resolved = NameResolver::new().resolve_program(program)?;
    TypeChecker::new().check(resolved)
}

fn check_inline_module(source: &str) -> Result<TypedModule, Vec<Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "staple-resource-dependencies-modules-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.sta");
    std::fs::write(&path, source).unwrap();
    let loaded = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_path(&path)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let _ = std::fs::remove_dir_all(&directory);
    let resolved = NameResolver::new().resolve_program(loaded)?;
    TypeChecker::new().check(resolved)
}

fn lower(source: &str) -> LoweredModule {
    let typed = check(source).unwrap_or_else(|errors| panic!("check: {errors:?}"));
    Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

#[cfg(unix)]
fn run(source: &str) -> std::process::Output {
    let lowered = lower(source);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-resource-dependencies-{}-{nonce}",
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
    output
}

fn effects<'a>(module: &'a TypedModule, name: &str) -> &'a staple_compiler::CheckedEffectSet {
    let suffix = format!(".{name}");
    let function = module
        .functions()
        .iter()
        .find(|function| function.name == name || function.name.ends_with(&suffix))
        .unwrap_or_else(|| panic!("missing function `{name}`"));
    &module.type_of_function(function.id).unwrap().effects
}

fn resource_names(effects: &staple_compiler::CheckedEffectSet) -> Vec<String> {
    effects
        .resources
        .iter()
        .map(|resource| resource.value_type.to_string())
        .collect()
}

#[test]
#[cfg(unix)]
fn body_with_database_uses_logger_in_both_provider_orders() {
    let output = run(r#"
use std.io.println
type Logger wrap (write: String -> ())
type Database with {Logger} wrap (path: String)
def log = (message: String) =>{Logger} () :: (resource Logger).write message
def query = (sql: String) =>{Database} () :: log "query: $sql"
with Logger = Logger (write: message => println "logger $message") {
    with Database = Database (path: "db") {
        query "one"
    }
}
with Database = Database (path: "db") {
    with Logger = Logger (write: message => println "outer $message") {
        query "two"
    }
}
"#);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "logger query: one\nouter query: two\n"
    );
}

#[test]
fn transitive_dependency_is_available() {
    let module = check(
        r#"
type C wrap ()
type B with {C} wrap ()
type A with {B} wrap ()
def use_c = () =>{C} () :: ()
def use_a = () =>{A} () :: use_c ()
"#,
    )
    .unwrap();
    let mut names = resource_names(effects(&module, "use_a"));
    names.sort();
    assert_eq!(names, ["A", "B", "C"]);
    assert_eq!(resource_names(effects(&module, "use_c")), ["C"]);
}

#[test]
fn closed_sets_are_equal_and_interchangeable_through_annotations() {
    check(
        r#"
type Logger wrap ()
type Database with {Logger} wrap ()
def database = () =>{Database} () :: ()
def both: () ->{Database, Logger} () = database
def back: () ->{Database} () = both
let stored: () ->{Database, Logger} () = database
let inferred = database
let narrowed: () ->{Database} () = inferred
"#,
    )
    .expect("closed effect sets should be equal");
}

#[test]
#[cfg(unix)]
fn an_inner_logger_provider_shadows_the_outer_one() {
    let output = run(r#"
use std.io.println
type Logger wrap (write: String -> ())
type Database with {Logger} wrap ()
def log = (message: String) =>{Logger} () :: (resource Logger).write message
def query = () =>{Database} () :: log "query"
with Logger = Logger (write: message => println "outer $message") {
    with Database = Database () {
        with Logger = Logger (write: message => println "inner $message") {
            query ()
        }
    }
}
"#);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "inner query\n");
}

#[test]
#[cfg(unix)]
fn mutable_dependencies_mutate_and_require_a_mutable_provider() {
    let source = r#"
use std.io.println
type Logger wrap (calls: I32)
type Database with {mut Logger} wrap ()
def bump = () =>{mut Logger} () :: { (resource Logger).calls = (resource Logger).calls + 1 }
def query = () =>{Database} () :: bump ()
let mut logger = Logger (calls: 0)
with mut Logger = logger {
    with Database = Database () {
        query ()
    }
}
println "${logger.calls}"
"#;
    let output = run(source);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n");

    let diagnostics = check(&source.replace("with mut Logger", "with Logger"))
        .expect_err("an immutable logger cannot satisfy `mut Logger`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use `with mut`")),
        "{diagnostics:?}"
    );
}

#[test]
#[cfg(unix)]
fn effect_polymorphism_passes_dependency_parameters() {
    let output = run(r#"
use std.io.println
type Logger wrap (write: String -> ())
type Database with {Logger} wrap ()
def log = (message: String) =>{Logger} () :: (resource Logger).write message
def query = () =>{Database} () :: log "query"
def twice: <effect E> (() ->{E} ()) ->{E} () = <effect E> f => { f (); f () }
with Logger = Logger (write: message => println message) {
    with Database = Database () {
        twice query
    }
}
"#);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "query\nquery\n");
}

#[test]
#[cfg(unix)]
fn generic_dependencies_substitute_their_arguments() {
    let output = run(r#"
use std.io.println
type Store<K> wrap (value: K)
type Cache<K> with {Store<K>} wrap ()
def stored = () =>{Store<I32>} I32 :: (resource Store<I32>).value
def cached = () =>{Cache<I32>} I32 :: stored ()
with Store<I32> = Store (value: 7) {
    with Cache<I32> = Cache () {
        println "${cached ()}"
    }
}
"#);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "7\n");
}

#[test]
fn a_type_parameter_may_be_the_dependency_itself() {
    check(
        r#"
type Store<K> wrap (value: K)
type Holder<K> with {K} wrap ()
def read = () =>{Holder<Store<I32>>} I32 :: (resource Store<I32>).value
"#,
    )
    .expect("a generic entry is validated after substitution");
}

#[test]
fn dependencies_cross_modules_and_re_exports() {
    let module = check_inline_module(
        r#"
pub mod
mod api {
    pub type Logger wrap pub (value: I32)
    pub type Database with {Logger} wrap ()
}
use api.(Logger, Database as ReexportedDatabase)
pub use api.Database
def read = () =>{ReexportedDatabase} I32 :: (resource Logger).value
"#,
    )
    .expect("imported and re-exported types keep their dependencies");
    let names = resource_names(effects(&module, "read"));
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(
        names.iter().any(|name| name.ends_with("Logger")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|name| name.ends_with("Database")),
        "{names:?}"
    );
}

#[test]
#[cfg(unix)]
fn named_database_body_reaches_the_implied_logger_by_type() {
    let output = run(r#"
use std.io.println
type Logger wrap (value: I32)
type Database with {Logger} wrap ()
def read = () =>{db: Database} I32 :: (resource Logger).value
with Logger = Logger (value: 9) {
    with Database = Database () {
        println "${read ()}"
    }
}
"#);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "9\n");
}

#[test]
#[cfg(unix)]
fn provider_initializer_runs_in_the_outer_scope() {
    let output = run(r#"
use std.io.println
type Logger wrap (value: I32)
type Database with {Logger} wrap (base: I32)
def open = (base: I32) =>{Logger} Database :: Database (base: base + (resource Logger).value)
def read = () =>{Database} I32 :: (resource Database).base
with Logger = Logger (value: 5) {
    with Database = open 2 {
        println "${read ()}"
    }
}
"#);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "7\n");
}

#[test]
fn explicit_empty_contract_reports_the_dependency_reason() {
    let diagnostics = check(
        r#"
type Logger wrap ()
type Database with {Logger} wrap ()
def log = () =>{Logger} () :: ()
def invalid: () ->{} () = () => {
    with Database = Database () {
        log ()
    }
}
"#,
    )
    .expect_err("the enclosing contract excludes the implied logger");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("Logger")
                && diagnostic.message.contains("Database")
                && diagnostic.message.contains("depends on")
        }),
        "{diagnostics:?}"
    );
}

#[test]
#[cfg(unix)]
fn trait_method_signatures_imply_dependencies() {
    // Trait signatures are resolved before most other passes, so this fails
    // if closing ever depends on dependency templates collected later.
    let output = run(r#"
use std.io.println
type Logger wrap (write: String -> ())
type Database with {Logger} wrap ()
trait Query T {
    run: T ->{Database} ()
}
type Q wrap ()
impl Query Q {
    run = q => (resource Logger).write "trait"
}
def go: () ->{Database} () = () => run (Q ())
with Logger = Logger (write: message => println message) {
    with Database = Database () {
        go ()
    }
}
"#);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "trait\n");
}

#[test]
fn rejects_representations_that_close_into_themselves() {
    // `Handler`'s field set closes into `{Database, Handler, Logger}`, the
    // same cycle as writing `Handler` in its own field's effect set.
    for source in [
        concat!(
            "type Logger wrap ()\n",
            "type Handler wrap (call: () ->{Database} ())\n",
            "type Database with {Handler, Logger} wrap ()\n",
        ),
        concat!(
            "type Logger wrap ()\n",
            "type Database with {Handler, Logger} wrap ()\n",
            "type Handler wrap (call: () ->{Database} ())\n",
        ),
    ] {
        let diagnostics = check(source).expect_err("the representation is cyclic");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("cyclic type definition involving `Handler`")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn rejects_self_and_transitive_cycles() {
    for source in [
        "type A with {A} wrap ()\n",
        "type A with {B} wrap ()\ntype B with {A} wrap ()\n",
    ] {
        let diagnostics = check(source).expect_err("resource dependency cycles must be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("cyclic resource dependency")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn rejects_state_effect_variables_and_named_entries() {
    for source in [
        "type A with {state} wrap ()\n",
        "type A{E} with {E} wrap ()\n",
        "type Logger wrap ()\ntype A with {log: Logger} wrap ()\n",
    ] {
        let diagnostics = check(source).expect_err("invalid dependency entries must be rejected");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("resource dependenc")
                    || diagnostic.message.contains("effect variable")
                    || diagnostic.message.contains("effect parameter `E`")
            }),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn rejects_non_resource_eligible_entries() {
    for source in [
        "type A with {(I32, I32)} wrap ()\n",
        concat!(
            "type Token opaque\n",
            "type A<T where ?Sized T> with {T} wrap ()\n",
            "def use_a = () =>{A<Token>} () :: ()\n",
        ),
    ] {
        let diagnostics = check(source).expect_err("ineligible dependency types must be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.message.contains("concrete, sized nominal type") }),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn substituted_dependency_errors_point_at_each_use() {
    let source = concat!(
        "type Cache<K> with {K} wrap ()\n",
        "def a: () ->{Cache<(I32, I32)>} () = () => ()\n",
        "def b = () => resource Cache<(I32, I32)>\n",
    );
    let diagnostics = check(source).expect_err("`(I32, I32)` is not a resource type");
    let mut lines = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("of `Cache<"))
        .map(|diagnostic| match &diagnostic.span {
            Span::User {
                location: Some(location),
                ..
            } => location.line,
            other => panic!("expected a user span, found {other:?}"),
        })
        .collect::<Vec<_>>();
    lines.sort();
    assert_eq!(lines, [2, 3], "{diagnostics:?}");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("declared at line 1, column 21")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_private_dependencies_of_public_types() {
    let diagnostics = check(
        r#"
type Logger wrap ()
pub type Database with {Logger} wrap ()
"#,
    )
    .expect_err("a public type cannot depend on a private resource");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("private type `Logger`")),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_dependencies_on_transparent_aliases() {
    for source in [
        "type Logger wrap ()\ntype Alias with {Logger} = Logger\n",
        "type Logger wrap ()\ntype Alias = Logger with {Logger}\n",
    ] {
        let diagnostics =
            check(source).expect_err("an alias takes its dependencies from its target");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .message
                    .contains("transparent alias cannot declare resource dependencies")
            }),
            "{source}: {diagnostics:?}"
        );
    }
}

#[test]
fn a_provider_does_not_discharge_its_dependencies() {
    let module = check(
        r#"
type Logger wrap ()
type Database with {Logger} wrap ()
def log = () =>{Logger} () :: ()
def inferred = () => {
    with Database = Database () {
        log ()
    }
}
"#,
    )
    .unwrap();
    assert_eq!(resource_names(effects(&module, "inferred")), ["Logger"]);
}

#[test]
fn type_declaration_items_expose_the_dependency_clause() {
    check(
        r#"
use std.syntax.(Item, Sequence, Type, TypeDeclarationItem, WrapperDeclaration)
type Logger wrap ()
macro @inspect: Item -> Item = item => match item {
    TypeDeclarationItem(WrapperDeclaration(), _, _, _, _, _, dependencies) => {
        let names: Sequence<Type> = dependencies
        item
    },
    _ => item,
}
@inspect
type Database with {Logger} wrap ()
def log = () =>{Logger} () :: ()
def query = () =>{Database} () :: log ()
"#,
    )
    .expect("modifier macros should see and preserve resource dependencies");
}

#[test]
fn quoted_type_declarations_keep_the_dependency_clause() {
    check(
        r#"
use std.syntax.(parse_quote, Item)
type Logger wrap ()
macro declare: () -> Item = () => parse_quote { type Database with {Logger} wrap () }
declare ()
def log = () =>{Logger} () :: ()
def query = () =>{Database} () :: log ()
"#,
    )
    .expect("quoting must preserve the dependency clause");
}
