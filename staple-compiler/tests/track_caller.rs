//! Runs programs that read `Location.caller ()` through `@track_caller`
//! functions and checks the reported source positions.

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn try_lower(source: &str) -> Result<LoweredModule, String> {
    try_lower_with(source, false)
}

fn try_lower_with(source: &str, dictionary_passing: bool) -> Result<LoweredModule, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .map_err(|error| format!("load: {error}"))?;
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(|errors| format!("resolve: {errors:?}"))?;
    let typed = TypeChecker::new()
        .check(resolved)
        .map_err(|errors| format!("check: {errors:?}"))?;
    Lowerer::new()
        .with_dictionary_pass_generic_calls(dictionary_passing)
        .lower(&typed)
        .map_err(|errors| format!("lower: {errors:?}"))
}

/// Builds and runs a program, returning its standard output.
#[cfg(unix)]
fn run(source: &str) -> String {
    run_with(source, false)
}

#[cfg(unix)]
fn run_with(source: &str, dictionary_passing: bool) -> String {
    let output = execute(source, dictionary_passing);
    assert!(
        output.status.success(),
        "execution: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Builds and runs a program, returning its exit status and output.
#[cfg(unix)]
fn execute(source: &str, dictionary_passing: bool) -> std::process::Output {
    let lowered =
        try_lower_with(source, dictionary_passing).unwrap_or_else(|error| panic!("{error}"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-track-caller-{}-{nonce}",
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

#[cfg(unix)]
fn run_lines(source: &str) -> Vec<String> {
    run(source).lines().map(str::to_owned).collect()
}

const PRELUDE: &str = concat!(
    "use std.io.println\n",
    "use std.source.(Location, track_caller)\n",
);

#[cfg(unix)]
#[test]
fn top_level_and_plain_function_locations() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "println \"${Location.caller ()}\"\n",
            "def plain = () => () :: {\n",
            "    println \"${Location.caller ()}\"\n",
            "}\n",
            "plain ()\n",
        )
    );
    assert_eq!(run_lines(&source), ["<stdin>.sta:3:12", "<stdin>.sta:5:16"]);
}

#[cfg(unix)]
#[test]
fn one_helper_reports_each_call_site() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def here = () => Location :: Location.caller ()\n",
            "println \"${here ()}\"\n",
            "println \"${here ()}\"\n",
            "let width = 3\n",
            "println \"${width}${here ()}\"\n",
        )
    );
    assert_eq!(
        run_lines(&source),
        ["<stdin>.sta:5:12", "<stdin>.sta:6:12", "3<stdin>.sta:8:20"]
    );
}

#[cfg(unix)]
#[test]
fn nested_helpers_forward_the_outermost_caller() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def inner = () => Location :: Location.caller ()\n",
            "@track_caller\n",
            "def middle = () => Location :: inner ()\n",
            "@track_caller\n",
            "def outer = () => Location :: middle ()\n",
            "println \"${outer ()}\"\n",
            "println \"${middle ()}\"\n",
        )
    );
    assert_eq!(
        run_lines(&source),
        ["<stdin>.sta:9:12", "<stdin>.sta:10:12"]
    );
}

#[cfg(unix)]
#[test]
fn generic_helpers_stay_monomorphised() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "use std.fmt.Display\n",
            "@track_caller\n",
            "def tagged: <T where Copy T, Display T> T -> String = <T where Copy T, Display T> value =>\n",
            "    \"${value}@${Location.caller ()}\"\n",
            "@track_caller\n",
            "def forwarded: <T where Copy T, Display T> T -> String = <T where Copy T, Display T> value =>\n",
            "    tagged value\n",
            "println (tagged 5)\n",
            "println (tagged.<String> \"x\")\n",
            "println (forwarded 7)\n",
        )
    );
    assert_eq!(
        run_lines(&source),
        [
            "5@<stdin>.sta:10:10",
            "x@<stdin>.sta:11:10",
            "7@<stdin>.sta:12:10"
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_function_value_fixes_the_site_at_the_reference() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def here = () => Location :: Location.caller ()\n",
            "let g = here\n",
            "println \"${g ()}\"\n",
            "println \"${g ()}\"\n",
            "let h: () -> Location = here\n",
            "println \"${h ()}\"\n",
        )
    );
    assert_eq!(
        run_lines(&source),
        ["<stdin>.sta:5:9", "<stdin>.sta:5:9", "<stdin>.sta:8:25"]
    );
}

#[cfg(unix)]
#[test]
fn recursion_reports_the_outermost_call() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def countdown = n: I32 => Location :: {\n",
            "    match n == 0 {\n",
            "        True => Location.caller (),\n",
            "        False => countdown (n - 1),\n",
            "    }\n",
            "}\n",
            "println \"${countdown 3}\"\n",
            "println \"${countdown 0}\"\n",
        )
    );
    assert_eq!(
        run_lines(&source),
        ["<stdin>.sta:10:12", "<stdin>.sta:11:12"]
    );
}

#[cfg(unix)]
#[test]
fn closures_inside_a_body_report_their_own_site() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def inner = () => Location :: Location.caller ()\n",
            "@track_caller\n",
            "def through_closure = () => Location :: {\n",
            "    let call_inner = () => inner ()\n",
            "    call_inner ()\n",
            "}\n",
            "println \"${through_closure ()}\"\n",
        )
    );
    assert_eq!(run_lines(&source), ["<stdin>.sta:7:28"]);
}

#[cfg(unix)]
#[test]
fn accessors_expose_the_parts() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "let at = Location.caller ()\n",
            "println \"${Location.file at}\"\n",
            "println \"${Location.line at}\"\n",
            "println \"${Location.column at}\"\n",
        )
    );
    assert_eq!(run_lines(&source), ["<stdin>.sta", "3", "10"]);
}

#[cfg(unix)]
#[test]
fn macro_generated_calls_report_the_outermost_invocation() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def here = () => Location :: Location.caller ()\n",
            "use std.syntax.(parse_quote)\n",
            "macro call_here = () => parse_quote { here () }\n",
            "println \"${call_here ()}\"\n",
        )
    );
    assert_eq!(run_lines(&source), ["<stdin>.sta:7:12"]);
}

const MARKER: &str = concat!(
    "type Marker = wrap I32\n",
    "companion Marker {\n",
    "    use std.source.(Location, track_caller)\n",
    "    @track_caller\n",
    "    pub def locate = marker: Marker => String :: {\n",
    "        let Marker value = marker\n",
    "        \"${value}:${Location.caller ()}\"\n",
    "    }\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn companion_functions_report_their_callers() {
    let source = format!(
        "{PRELUDE}{MARKER}{}",
        "println (Marker.locate (Marker 1))\n"
    );
    assert_eq!(run_lines(&source), ["1:<stdin>.sta:12:10"]);
}

#[cfg(unix)]
#[test]
fn pipes_report_the_position_of_the_function() {
    let source = format!(
        "{PRELUDE}{MARKER}{}",
        concat!("let piped = Marker 2 |> locate\n", "println piped\n",)
    );
    assert_eq!(run_lines(&source), ["2:<stdin>.sta:12:25"]);
}

#[cfg(unix)]
#[test]
fn generic_functions_can_be_values_at_a_concrete_type() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "use std.fmt.Display\n",
            "@track_caller\n",
            "def tagged: <T where Copy T, Display T> T -> String = <T where Copy T, Display T> value =>\n",
            "    \"${value}@${Location.caller ()}\"\n",
            "let g: I32 -> String = tagged\n",
            "println (g 4)\n",
        )
    );
    assert_eq!(run_lines(&source), ["4@<stdin>.sta:7:24"]);
}

#[test]
fn a_polymorphic_value_use_is_rejected() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "use std.fmt.Display\n",
            "@track_caller\n",
            "def tagged: <T where Copy T, Display T> T -> String = <T where Copy T, Display T> value =>\n",
            "    \"${value}@${Location.caller ()}\"\n",
            "let g: <T where Copy T, Display T> T -> String = tagged\n",
        )
    );
    match try_lower(&source) {
        Ok(_) => panic!("a polymorphic use of a tracked function should be rejected"),
        Err(error) => assert!(
            error.contains("cannot be used as a polymorphic value"),
            "{error}"
        ),
    }
}

#[cfg(unix)]
#[test]
fn only_callee_references_in_the_own_body_forward() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def inner = () => Location :: Location.caller ()\n",
            "@track_caller\n",
            "def via_value = () => Location :: {\n",
            "    let again = inner\n",
            "    again ()\n",
            "}\n",
            "println \"${via_value ()}\"\n",
        )
    );
    assert_eq!(run_lines(&source), ["<stdin>.sta:7:17"]);
}

#[cfg(unix)]
#[test]
fn mutual_and_self_value_recursion_forward() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "@track_caller\n",
            "def ping = n: I32 => Location :: match n == 0 {\n",
            "    True => Location.caller (),\n",
            "    False => pong (n - 1),\n",
            "}\n",
            "@track_caller\n",
            "def pong = n: I32 => Location :: ping n\n",
            "@track_caller\n",
            "def selfy = n: I32 => Location :: {\n",
            "    let again = selfy\n",
            "    match n == 0 {\n",
            "        True => Location.caller (),\n",
            "        False => again (n - 1),\n",
            "    }\n",
            "}\n",
            "println \"${ping 3}\"\n",
            "println \"${selfy 2}\"\n",
        )
    );
    assert_eq!(
        run_lines(&source),
        ["<stdin>.sta:18:12", "<stdin>.sta:19:12"]
    );
}

#[test]
fn coroutine_bodies_in_a_tracked_function_compile_with_their_own_site() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "use std.coroutine.Coroutine\n",
            "@track_caller\n",
            "def inner = () => Location :: Location.caller ()\n",
            "@track_caller\n",
            "def delayed = () => Coroutine Location :: coro { inner () }\n",
        )
    );
    try_lower(&source).unwrap_or_else(|error| panic!("{error}"));
}

#[cfg(unix)]
#[test]
fn the_documented_example_runs() {
    let source = concat!(
        "use std.io.(IO, println)\n",
        "use std.source.(Location, track_caller)\n",
        "\n",
        "@track_caller\n",
        "pub def check = condition: Bool =>{IO} () :: match condition {\n",
        "    True => (),\n",
        "    False => println \"check failed at ${Location.caller ()}\",\n",
        "}\n",
        "\n",
        "check (1 > 2)\n",
    );
    assert_eq!(run_lines(source), ["check failed at <stdin>.sta:10:1"]);
}

/// The hidden dictionary-passing lowering excludes tracked functions from
/// dynamic emission, so they keep their static path and report the same sites.
#[cfg(unix)]
#[test]
fn dictionary_passing_keeps_tracked_functions_static() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "use std.fmt.Display\n",
            "@track_caller\n",
            "def tagged: <T where Copy T, Display T> T -> String = <T where Copy T, Display T> value =>\n",
            "    \"${value}@${Location.caller ()}\"\n",
            "def relay: <T where Copy T, Display T> T -> String = <T where Copy T, Display T> value =>\n",
            "    tagged value\n",
            "println (tagged 5)\n",
            "println (relay 6)\n",
        )
    );
    let output = run_with(&source, true);
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        ["5@<stdin>.sta:9:10", "6@<stdin>.sta:8:5"]
    );
}

#[test]
#[cfg(unix)]
fn panic_reports_the_location_of_its_call() {
    let output = execute(
        concat!(
            "use std.io.println\n",
            "println \"before\"\n",
            "{ panic \"stop\" }\n",
        ),
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "before\npanic: stop at <stdin>.sta:3:3\n"
    );
}

#[test]
#[cfg(unix)]
fn panic_inside_a_tracked_function_reports_its_caller() {
    let output = execute(
        concat!(
            "use std.source.(track_caller)\n",
            "@track_caller\n",
            "def expect_positive = (value: I32) => I32 :: if (value > 0) { value } else {\n",
            "    panic \"expected a positive value\"\n",
            "}\n",
            "let _ = expect_positive 1\n",
            "let _ = expect_positive (-1)\n",
        ),
        false,
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "panic: expected a positive value at <stdin>.sta:7:9\n"
    );
}

const TRACKED_TRAIT: &str = concat!(
    "trait Where T {\n",
    "    @track_caller\n",
    "    site: T -> String = value => \"default@${Location.caller ()}\"\n",
    "}\n",
    "type A\n",
    "type B\n",
    "impl Where A {}\n",
    "impl Where B { site = value => \"b@${Location.caller ()}\" }\n",
    "def relay = <T where Where T> value: T => String :: Where.site value\n",
    "@track_caller\n",
    "def forward = <T where Where T> value: T => String :: Where.site value\n",
    "println (Where.site A)\n",
    "println (B |> site)\n",
    "println (relay A)\n",
    "println (forward B)\n",
    "let f: B -> String = Where.site\n",
    "println (f B)\n",
);

const TRACKED_TRAIT_SITES: [&str; 5] = [
    "default@<stdin>.sta:14:10",
    "b@<stdin>.sta:15:15",
    "default@<stdin>.sta:11:53",
    "b@<stdin>.sta:17:10",
    "b@<stdin>.sta:18:22",
];

/// A `@track_caller` trait member tracks its default and every
/// implementation: direct and piped calls report their own site, a call in
/// an untracked generic body reports that body, a tracked caller forwards
/// its own caller, and a value fixes the site at the reference.
#[cfg(unix)]
#[test]
fn trait_members_report_their_callers() {
    let source = format!("{PRELUDE}{TRACKED_TRAIT}");
    assert_eq!(run_lines(&source), TRACKED_TRAIT_SITES);
}

#[cfg(unix)]
#[test]
fn dictionary_passing_keeps_tracked_trait_calls_static() {
    let source = format!("{PRELUDE}{TRACKED_TRAIT}");
    let output = run_with(&source, true);
    assert_eq!(output.lines().collect::<Vec<_>>(), TRACKED_TRAIT_SITES);
}

#[test]
fn implementation_members_cannot_be_tracked_themselves() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "trait Where T { site: T -> String }\n",
            "impl Where I32 {\n",
            "    @track_caller\n",
            "    site = value => \"here\"\n",
            "}\n",
        )
    );
    match try_lower(&source) {
        Ok(_) => panic!("`@track_caller` on an implementation member should be rejected"),
        Err(error) => assert!(
            error.contains("implementation members inherit it"),
            "{error}"
        ),
    }
}

#[test]
fn a_polymorphic_tracked_trait_member_value_is_rejected() {
    let source = format!(
        "{PRELUDE}{}",
        concat!(
            "trait Where T {\n",
            "    @track_caller\n",
            "    site: T -> String = value => \"${Location.caller ()}\"\n",
            "}\n",
            "impl Where I32 {}\n",
            "let g = Where.site\n",
        )
    );
    match try_lower(&source) {
        Ok(_) => panic!("a polymorphic use of a tracked trait member should be rejected"),
        Err(error) => assert!(
            error.contains("cannot be used as a polymorphic value"),
            "{error}"
        ),
    }
}

#[cfg(unix)]
#[test]
fn try_unwraps_results_options_and_wider_sums() {
    let source = concat!(
        "use std.io.println\n",
        "let good: Result I32 String = Ok 1\n",
        "let wide: Ok I32 | String | Bool = Ok 2\n",
        "let some: Option I32 = Some 3\n",
        "let parts = (good |> unwrap, Try.unwrap wide, some |> expect \"needs a value\")\n",
        "println \"${parts.0} ${parts.1} ${parts.2}\"\n",
    );
    assert_eq!(run_lines(source), ["1 2 3"]);
}

#[cfg(unix)]
#[test]
fn try_unwrap_and_expect_panic_at_their_caller() {
    let unwrap = execute(
        concat!(
            "let none: Option I32 = None\n",
            "let _ = none |> unwrap\n",
        ),
        false,
    );
    assert_eq!(unwrap.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(unwrap.stdout).unwrap(),
        "panic: called `unwrap` on a failure at <stdin>.sta:2:17\n"
    );
    let expect = execute(
        concat!(
            "let bad: Result I32 String = \"boom\"\n",
            "let _ = Try.expect (bad; \"loading config\")\n",
        ),
        false,
    );
    assert_eq!(expect.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(expect.stdout).unwrap(),
        "panic: loading config at <stdin>.sta:2:9\n"
    );
}
