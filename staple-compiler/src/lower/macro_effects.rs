//! Reachability-based compile-time effect enforcement for macro libraries.
//!
//! Lowering seeds non-generic functions of a package even when no macro calls
//! them. Walking the lowered call graph (including functions whose address is
//! taken as a [`LoweredCallableValue`], trait methods, and generated artifacts)
//! from each macro entry function finds only the forbidden functions and extern
//! bindings that the macro can actually reach.
//!
//! A list of names can only cover the standard library's own bindings: any
//! package can declare `extern "c" { open: ... }`. So macro code may reach a
//! foreign function only if the standard library declares it, and only if it
//! isn't one of the forbidden ones below.

use std::collections::{HashMap, HashSet, VecDeque};

use staple_syntax::{Diagnostic, Span};

use crate::specialization::ArtifactOrdinal;

use super::{FunctionInstanceId, LoweredCallableTarget, LoweredModule};

/// Standard-library functions that compiled macro code may not reach.
pub(crate) const FORBIDDEN_FUNCTIONS: &[&str] = &[
    // std.fs
    "std.fs.metadata",
    "std.fs.symlink_metadata",
    "std.fs.exists",
    "std.fs.File.open",
    "std.fs.File.create",
    "std.fs.File.open_with",
    "std.fs.File.metadata",
    "std.fs.File.read_into",
    "std.fs.File.read_to_end",
    "std.fs.File.write_all",
    "std.fs.File.write_string",
    "std.fs.File.flush",
    "std.fs.File.sync",
    "std.fs.File.seek",
    "std.fs.File.close",
    "std.fs.read",
    "std.fs.read_to_string",
    "std.fs.write",
    "std.fs.write_string",
    "std.fs.append_string",
    "std.fs.copy",
    "std.fs.create_dir",
    "std.fs.remove_file",
    "std.fs.remove_dir",
    "std.fs.rename",
    "std.fs.create_dir_all",
    "std.fs.canonicalize",
    "std.fs.current_dir",
    "std.fs.DirEntry.metadata",
    "std.fs.read_dir",
    "std.fs.remove_dir_all",
    // std.io (standard input)
    "std.io.read_line",
    "std.io.Stdin.read_byte",
    // std.rand (OS entropy seeding)
    "std.rand.fill_entropy",
    "std.rand.Rng.from_entropy",
    "std.rand.HashKey.from_entropy",
];

/// Foreign (`extern`) bindings declared by the standard library that compiled
/// macro code may not reach. `std.process` has no process spawning, so it adds
/// nothing here; foreign functions declared outside the standard library are
/// rejected whatever their names.
pub(crate) const FORBIDDEN_EXTERNS: &[&str] = &[
    // std.fs
    "__staple_fs_errno",
    "__staple_fs_errno_value",
    "__staple_fs_open",
    "__staple_fs_stat",
    "__staple_fs_fstat",
    "__staple_fs_dir_open",
    "__staple_fs_dir_next",
    "__staple_fs_dir_close",
    "__staple_fs_rename",
    "fopen",
    "fclose",
    "fread",
    "fwrite",
    "fseeko",
    "ftello",
    "fileno",
    "fsync",
    "mkdir",
    "rmdir",
    "unlink",
    "realpath",
    "getcwd",
    // std.io (standard input)
    "__staple_io_read_byte",
    // std.rand (OS entropy seeding)
    "getentropy",
];

/// One macro entry function whose effects [`LoweredModule::check_macro_effects`]
/// checks.
pub struct MacroEffectEntry<'a> {
    /// The macro's name, for diagnostics.
    pub macro_name: &'a str,
    /// The lowered instance symbol of the macro's entry function.
    pub entry_symbol: &'a str,
    /// Where diagnostics are reported.
    pub span: Span,
}

impl LoweredModule {
    /// Checks that none of `entries` can reach a forbidden function or extern
    /// binding in the lowered call graph. Each reached target is reported once
    /// per macro, with its shortest chain. An entry symbol the program doesn't
    /// have is an internal error, so a wrong symbol can't pass unchecked.
    pub fn check_macro_effects(&self, entries: &[MacroEffectEntry<'_>]) -> Vec<Diagnostic> {
        let view = self.program.emission_view();
        let standard_library = self
            .packages()
            .iter()
            .find(|package| package.name == "std")
            .map(|package| package.id);
        let by_symbol: HashMap<&str, FunctionInstanceId> = view
            .instances()
            .map(|(id, instance)| (instance.name.as_str(), id))
            .collect();

        let mut diagnostics = Vec::new();
        for MacroEffectEntry {
            macro_name,
            entry_symbol,
            span,
        } in entries
        {
            let Some(&entry_id) = by_symbol.get(entry_symbol) else {
                diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!(
                        "internal error: macro `{macro_name}` has no lowered entry `{entry_symbol}` to check"
                    ),
                ));
                continue;
            };
            check_entry_reachability(
                &view,
                standard_library,
                macro_name,
                entry_id,
                span,
                &mut diagnostics,
            );
        }
        diagnostics
    }
}

fn check_entry_reachability(
    view: &super::emission::EmissionView<'_>,
    standard_library: Option<staple_project::PackageId>,
    macro_name: &str,
    entry_id: FunctionInstanceId,
    span: &Span,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    let mut parent: HashMap<FunctionInstanceId, Option<FunctionInstanceId>> = HashMap::new();
    let mut reported = HashSet::new();

    queue.push_back(entry_id);
    visited.insert(entry_id);
    parent.insert(entry_id, None);

    while let Some(current_id) = queue.pop_front() {
        let Some(instance) = view.instance(current_id) else {
            continue;
        };

        if current_id != entry_id
            && let Some(qualified) = instance_qualified_name(view, instance)
            && is_forbidden_function(&qualified)
        {
            if reported.insert(qualified.clone()) {
                let chain = format_chain(view, entry_id, current_id, &parent, None);
                diagnostics.push(Diagnostic::new(
                    span.clone(),
                    format!("macro `{macro_name}` reaches {chain}, which macro code can't use"),
                ));
            }
            continue;
        }

        if let Some(body) = &instance.body {
            let mut externs = Vec::new();
            for (_, call) in body.calls.iter() {
                if let LoweredCallableTarget::ExternalFunction { symbol } = &call.target {
                    externs.push(*symbol);
                }
            }
            for (_, cv) in body.callable_values.iter() {
                if let LoweredCallableTarget::ExternalFunction { symbol } = &cv.target {
                    externs.push(*symbol);
                }
            }
            for symbol_id in externs {
                let Some(symbol) = view.symbol(symbol_id) else {
                    continue;
                };
                let declared_by_std = standard_library.is_some()
                    && view.module_package(symbol.module) == standard_library;
                let reason = if !declared_by_std {
                    ", a foreign function the standard library doesn't declare,"
                } else if is_forbidden_extern(&symbol.name) {
                    ","
                } else {
                    continue;
                };
                if reported.insert(symbol.name.clone()) {
                    let chain =
                        format_chain(view, entry_id, current_id, &parent, Some(&symbol.name));
                    diagnostics.push(Diagnostic::new(
                        span.clone(),
                        format!(
                            "macro `{macro_name}` reaches {chain}{reason} which macro code can't use"
                        ),
                    ));
                }
            }
        }

        let mut next_instances = Vec::new();
        for dep in &instance.dependencies {
            next_instances.push(dep.instance);
        }
        let mut artifact_queue: VecDeque<ArtifactOrdinal> = instance
            .artifacts
            .iter()
            .map(|dep| dep.artifact)
            .collect();
        let mut seen_artifacts = HashSet::new();
        while let Some(ordinal) = artifact_queue.pop_front() {
            if !seen_artifacts.insert(ordinal) {
                continue;
            }
            if let Some(artifact) = view.artifact(ordinal) {
                for dep in &artifact.instances {
                    next_instances.push(dep.instance);
                }
                for dep in &artifact.artifacts {
                    artifact_queue.push_back(dep.artifact);
                }
            }
        }

        for next_id in next_instances {
            if visited.insert(next_id) {
                parent.insert(next_id, Some(current_id));
                queue.push_back(next_id);
            }
        }
    }
}

fn instance_qualified_name(
    view: &super::emission::EmissionView<'_>,
    instance: &super::worklist::LoweredFunctionInstance,
) -> Option<String> {
    let function = view.function(instance.template)?;
    if function.name.is_empty() {
        return None;
    }
    let qualified = function
        .name
        .strip_prefix("__staple_m")
        .unwrap_or(&function.name);
    // An operator or a macro's own function is named `operator.<hex>`, after
    // the module path when it has one.
    let (module, operator) = match qualified.rfind("operator.") {
        Some(at) if at == 0 || qualified[..at].ends_with('.') => (&qualified[..at], &qualified[at + 9..]),
        _ => return Some(qualified.to_owned()),
    };
    let Some(decoded) = decode_hex(operator) else {
        return Some(qualified.to_owned());
    };
    // The macro's entry and the macro function it wraps are the macro itself,
    // which the diagnostic already names.
    if decoded.starts_with("entry#") || decoded.starts_with("macro#") {
        return None;
    }
    Some(format!("{module}{decoded}"))
}

fn decode_hex(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let s = std::str::from_utf8(pair).ok()?;
        let byte = u8::from_str_radix(s, 16).ok()?;
        bytes.push(byte);
    }
    String::from_utf8(bytes).ok()
}

/// The chain from the entry to `end_id`, then `callee` when `end_id` calls a
/// foreign function. The entry and the macro function it wraps are left out.
fn format_chain(
    view: &super::emission::EmissionView<'_>,
    entry_id: FunctionInstanceId,
    end_id: FunctionInstanceId,
    parent: &HashMap<FunctionInstanceId, Option<FunctionInstanceId>>,
    callee: Option<&str>,
) -> String {
    let mut nodes = Vec::new();
    let mut cursor = Some(end_id);
    while let Some(id) = cursor {
        nodes.push(id);
        cursor = parent.get(&id).copied().flatten();
    }
    nodes.reverse();

    let mut steps: Vec<String> = nodes
        .into_iter()
        .filter(|&id| id != entry_id)
        .filter_map(|id| view.instance(id))
        .filter_map(|instance| instance_qualified_name(view, instance))
        .map(|name| format!("`{name}`"))
        .collect();
    if let Some(name) = callee {
        steps.push(format!("`{name}`"));
    }
    steps.join(" -> ")
}

fn is_forbidden_function(name: &str) -> bool {
    FORBIDDEN_FUNCTIONS.contains(&name)
}

fn is_forbidden_extern(name: &str) -> bool {
    FORBIDDEN_EXTERNS.contains(&name)
}

#[cfg(test)]
mod tests {
    use staple_syntax::Span;

    use super::MacroEffectEntry;

    use crate::macro_expand::check_program::{CheckMode, check_programs, entry_function_name};
    use crate::{Lowerer, NameResolver, ProgramLoader, TypeChecker};

    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate is in workspace")
            .to_path_buf()
    }

    fn lower_check_program(source: &str) -> (crate::Program, crate::LoweredModule) {
        let root = workspace_root();
        let stdlib = root.join("stdlib");
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib)
            .load_source(source, &root)
            .expect("source loads");
        let (expanded, analysis, _) =
            crate::macro_expand::expand_program_checked(program, true, Some(CheckMode::All), false)
                .expect("source expands");
        let snapshot = analysis.check.expect("check snapshot");
        let mut next = analysis.next_syntax_id;
        let (_, _, check_program, _) = check_programs(&expanded, &snapshot, &mut next)
            .into_iter()
            .find(|(package, _, _, _)| package.is_none())
            .expect("entry check program");
        let (resolved, diagnostics) =
            NameResolver::pre_expanded(check_program.next_syntax_id())
                .resolve_program_recovering(check_program);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let typed = TypeChecker::new()
            .check(resolved.expect("resolved"))
            .expect("typechecked");
        let lowered = Lowerer::new().lower(&typed).expect("lowered");
        (expanded, lowered)
    }

    /// The effects reported for each macro of `source`'s entry module.
    fn macro_effects(source: &str, macros: &[&str]) -> Vec<(String, Vec<String>)> {
        let (expanded, lowered) = lower_check_program(source);
        let entry_module = expanded.entry();
        macros
            .iter()
            .map(|macro_name| {
                let decl = expanded
                    .module(entry_module)
                    .syntax
                    .items
                    .iter()
                    .find_map(|item| match item {
                        staple_syntax::Item::MacroDeclaration(decl) if decl.name == *macro_name => {
                            Some(decl)
                        }
                        _ => None,
                    })
                    .expect("macro exists");
                let mangled = crate::resolve::mangle_function_name(&entry_function_name(decl));
                let symbol = lowered
                    .planned_instance_names(&mangled)
                    .into_iter()
                    .next()
                    .expect("entry lowered");
                let messages = lowered
                    .check_macro_effects(&[MacroEffectEntry {
                        macro_name,
                        entry_symbol: &symbol,
                        span: Span::Compiler,
                    }])
                    .into_iter()
                    .map(|diagnostic| diagnostic.message)
                    .collect();
                (macro_name.to_string(), messages)
            })
            .collect()
    }

    #[test]
    fn macro_effects_follow_reachable_code() {
        // A macro's entry function has no effects, so it can't call `std`
        // functions that need `IO`. A foreign function needs no effect, which
        // is how macro code could reach the operating system.
        let source = concat!(
            "use std.process.panic\n",
            "use std.syntax.*\n",
            "extern \"c\" { open: (I32, I32) -> I32 }\n",
            "def open_file = () => () :: {\n",
            "    let _ = open (0, 0)\n",
            "}\n",
            "def never_called = () => () :: open_file ()\n",
            "macro own_binding: Expr -> Expr = value => {\n",
            "    let _ = open (0, 0)\n",
            "    parse_quote { $value }\n",
            "}\n",
            "macro through_helper: Expr -> Expr = value => {\n",
            "    open_file ()\n",
            "    parse_quote { $value }\n",
            "}\n",
            "macro fails: Expr -> Expr = _value => panic \"bad input\"\n",
            "let x = 1\n",
        );
        let effects = macro_effects(source, &["own_binding", "through_helper", "fails"]);
        assert_eq!(
            effects,
            vec![
                (
                    "own_binding".to_owned(),
                    vec![
                        "macro `own_binding` reaches `open`, a foreign function the standard \
                         library doesn't declare, which macro code can't use"
                            .to_owned()
                    ],
                ),
                (
                    "through_helper".to_owned(),
                    vec![
                        "macro `through_helper` reaches `open_file` -> `open`, a foreign \
                         function the standard library doesn't declare, which macro code \
                         can't use"
                            .to_owned()
                    ],
                ),
                // `panic` writes to standard error through the standard
                // library's own bindings, which is allowed, and `never_called`
                // reaches `open` only from dead code.
                ("fails".to_owned(), Vec::new()),
            ]
        );
    }

    #[test]
    fn rejects_reachable_std_fs_read_to_string_with_chain() {
        let root = workspace_root();
        let stdlib = root.join("stdlib");
        let source = concat!(
            "use std.fs.(Path, read_to_string)\n",
            "use std.io.IO\n",
            "def read_helper = () =>{IO} () :: {\n",
            "    let _ = read_to_string (Path \"/tmp/missing\")\n",
            "}\n",
            "def entry = () =>{IO} () :: read_helper ()\n",
            "def main = () =>{IO} () :: ()\n",
        );
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib)
            .load_source(source, &root)
            .expect("source loads");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("resolved");
        let typed = TypeChecker::new().check(resolved).expect("typechecked");
        let lowered = Lowerer::new().lower(&typed).expect("lowered");
        let entry_sym = lowered
            .planned_instance_names(&crate::resolve::mangle_function_name("entry"))
            .into_iter()
            .next()
            .expect("entry lowered");
        let diags = lowered.check_macro_effects(&[MacroEffectEntry {
            macro_name: "m",
            entry_symbol: &entry_sym,
            span: Span::Compiler,
        }]);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(
            diags[0].message,
            "macro `m` reaches `read_helper` -> `std.fs.read_to_string`, which macro code can't use"
        );
    }
}
