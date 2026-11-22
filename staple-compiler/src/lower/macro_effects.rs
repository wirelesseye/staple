//! Reachability-based compile-time effect enforcement for macro libraries.
//!
//! Lowering seeds non-generic functions of a package even when no macro calls
//! them. Walking the lowered call graph (including functions whose address is
//! taken as a [`LoweredCallableValue`], trait methods, and generated artifacts)
//! from each macro entry function finds only the forbidden functions and extern
//! bindings that the macro can actually reach.

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
    // std.process (process spawning)
    "std.process.spawn",
    "std.process.Command.spawn",
    "std.process.Command.output",
    "std.process.Command.status",
    // std.rand (OS entropy seeding)
    "std.rand.fill_entropy",
    "std.rand.Rng.from_entropy",
    "std.rand.HashKey.from_entropy",
];

/// Foreign (`extern`) bindings that compiled macro code may not reach.
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
    // std.process (process spawning)
    "system",
    "popen",
    "fork",
    "vfork",
    "posix_spawn",
    "posix_spawnp",
    "execve",
    "execv",
    "execvp",
    "execvpe",
    "execl",
    "execlp",
    "execle",
    // std.rand (OS entropy seeding)
    "getentropy",
];

impl LoweredModule {
    /// Checks that none of `entries` (`(macro_name, entry_symbol, span)`) can
    /// reach a forbidden function or extern binding in the lowered call graph.
    /// Each reached target is reported once per macro, with its shortest chain.
    pub fn check_macro_effects(&self, entries: &[(&str, &str, Span)]) -> Vec<Diagnostic> {
        let view = self.program.emission_view();
        let by_symbol: HashMap<&str, FunctionInstanceId> = view
            .instances()
            .map(|(id, instance)| (instance.name.as_str(), id))
            .collect();

        let mut diagnostics = Vec::new();
        for (macro_name, entry_symbol, span) in entries {
            let Some(&entry_id) = by_symbol.get(entry_symbol) else {
                continue;
            };
            check_entry_reachability(&view, macro_name, entry_id, span, &mut diagnostics);
        }
        diagnostics
    }
}

fn check_entry_reachability(
    view: &super::emission::EmissionView<'_>,
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
                let chain = format_chain(view, entry_id, current_id, &parent, Some(&qualified));
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
                if let Some(symbol) = view.symbol(symbol_id)
                    && is_forbidden_extern(&symbol.name)
                    && reported.insert(symbol.name.clone())
                {
                    let chain =
                        format_chain(view, entry_id, current_id, &parent, Some(&symbol.name));
                    diagnostics.push(Diagnostic::new(
                        span.clone(),
                        format!(
                            "macro `{macro_name}` reaches {chain}, which macro code can't use"
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
    if let Some(qualified) = function.name.strip_prefix("__staple_m") {
        return Some(qualified.to_owned());
    }
    if let Some(hex) = function.name.strip_prefix("operator.")
        && let Some(decoded) = decode_hex(hex)
    {
        if decoded.starts_with("entry#") || decoded.starts_with("macro#") {
            return None;
        }
        return Some(decoded);
    }
    Some(function.name.clone())
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

fn format_chain(
    view: &super::emission::EmissionView<'_>,
    entry_id: FunctionInstanceId,
    end_id: FunctionInstanceId,
    parent: &HashMap<FunctionInstanceId, Option<FunctionInstanceId>>,
    terminal_override: Option<&str>,
) -> String {
    let mut nodes = Vec::new();
    let mut cursor = Some(end_id);
    while let Some(id) = cursor {
        nodes.push(id);
        cursor = parent.get(&id).copied().flatten();
    }
    nodes.reverse();

    let mut steps = Vec::new();
    for (index, &id) in nodes.iter().enumerate() {
        if id == entry_id {
            continue;
        }
        let is_last = index + 1 == nodes.len();
        if is_last && let Some(name) = terminal_override {
            steps.push(format!("`{name}`"));
            continue;
        }
        if let Some(instance) = view.instance(id)
            && let Some(name) = instance_qualified_name(view, instance)
        {
            steps.push(format!("`{name}`"));
        }
    }
    if steps.is_empty()
        && let Some(name) = terminal_override
    {
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

    #[test]
    fn rejects_reachable_forbidden_extern_and_accepts_dead_code_and_stderr() {
        let source = concat!(
            "use std.process.panic\n",
            "use std.syntax.*\n",
            "extern \"c\" { __staple_fs_errno: () -> I32 }\n",
            "def open_helper = () => () :: {\n",
            "    let _ = __staple_fs_errno ()\n",
            "}\n",
            "def dead_helper = () => () :: {\n",
            "    let _ = __staple_fs_errno ()\n",
            "}\n",
            "macro bad_reader: Expr -> Expr = _value => {\n",
            "    open_helper ()\n",
            "    parse_quote { 1 }\n",
            "}\n",
            "macro ok_stderr: Expr -> Expr = _value => panic \"diagnostic on stderr\"\n",
            "let x = 1\n",
        );
        let (expanded, lowered) = lower_check_program(source);
        let entry_module = expanded.entry();
        let entry_for = |macro_name: &str| {
            let decl = expanded
                .module(entry_module)
                .syntax
                .items
                .iter()
                .find_map(|item| match item {
                    staple_syntax::Item::MacroDeclaration(decl) if decl.name == macro_name => {
                        Some(decl)
                    }
                    _ => None,
                })
                .expect("macro exists");
            let mangled = crate::resolve::mangle_function_name(&entry_function_name(decl));
            lowered
                .planned_instance_names(&mangled)
                .into_iter()
                .next()
                .expect("entry lowered")
        };

        let bad_sym = entry_for("bad_reader");
        let ok_sym = entry_for("ok_stderr");

        let bad_diags = lowered.check_macro_effects(&[("bad_reader", &bad_sym, Span::Compiler)]);
        assert_eq!(bad_diags.len(), 1, "{bad_diags:?}");
        assert_eq!(
            bad_diags[0].message,
            "macro `bad_reader` reaches `__staple_fs_errno`, which macro code can't use"
        );

        let ok_diags = lowered.check_macro_effects(&[("ok_stderr", &ok_sym, Span::Compiler)]);
        assert!(ok_diags.is_empty(), "{ok_diags:?}");
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
        let diags = lowered.check_macro_effects(&[("m", &entry_sym, Span::Compiler)]);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(
            diags[0].message,
            "macro `m` reaches `read_helper` -> `std.fs.read_to_string`, which macro code can't use"
        );
    }
}
