//! Builds the program that checks macro code as ordinary code.
//!
//! The program mirrors what compiling a package's macros needs: every
//! dependency package whole, after expansion and including its compile-time
//! items, and from the package being expanded only the items in the closures
//! being checked. A name a closure misses is then a resolution error.

use std::collections::{BTreeSet, HashMap, HashSet};

use staple_project::PackageId;
use staple_syntax::{
    Binding, BindingKind, Diagnostic, Item, MacroDeclaration, SelectedImport, Span, Syntax,
    SyntaxId, UseKind, Visibility,
};

use super::closure::{ItemId, MacroClosure, item_kind};
use super::{MacroAnalysis, MacroExpander, MacroKind};
use crate::{ModuleId, NameResolver, Program, TypeChecker};

/// Which macros have their closures checked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckMode {
    /// The macros that were invoked.
    Invoked,
    /// Every macro declared in the program.
    All,
}

/// Reads `STAPLE_CHECK_MACRO_CLOSURES`: `1` checks the closures of invoked
/// macros, `all` those of every macro. It is not a public flag.
pub(crate) fn check_mode() -> Option<CheckMode> {
    match std::env::var("STAPLE_CHECK_MACRO_CLOSURES").ok()?.as_str() {
        "all" => Some(CheckMode::All),
        "" | "0" => None,
        _ => Some(CheckMode::Invoked),
    }
}

/// A macro whose closure is checked.
pub(crate) struct CheckedMacro {
    pub(crate) name: String,
    pub(crate) module: ModuleId,
    pub(crate) closure: MacroClosure,
}

/// A closure item as expansion left it.
pub(crate) struct ClosureGroup {
    pub(crate) module: ModuleId,
    pub(crate) position: usize,
    pub(crate) items: Vec<Item>,
}

/// What expansion keeps for the check, which the program no longer holds once
/// compile-time items are dropped.
pub(crate) struct CheckSnapshot {
    /// Every module's expanded items, compile-time items included.
    pub(crate) modules: Vec<Vec<Item>>,
    /// The expanded form of each item some closure contains.
    pub(crate) groups: HashMap<ItemId, ClosureGroup>,
    pub(crate) macros: Vec<CheckedMacro>,
}

impl MacroExpander {
    /// Collects the closures to check and the items they contain. Runs after
    /// the module walk, before the index is released.
    pub(super) fn check_snapshot(&mut self, mode: CheckMode, modules: usize) -> CheckSnapshot {
        let mut declared = self
            .definitions
            .values()
            .filter(|definition| matches!(definition.kind, MacroKind::User(_)))
            .map(|definition| {
                (
                    definition.declaration.syntax.id,
                    definition.key.name.clone(),
                    definition.key.module,
                )
            })
            .collect::<Vec<_>>();
        declared.sort();
        let mut macros = Vec::new();
        for (start, name, module) in declared {
            let closure = match self.macro_closures.get(&start) {
                Some(closure) => closure.clone(),
                None if mode == CheckMode::All => self.macro_closure(start),
                None => continue,
            };
            macros.push(CheckedMacro {
                name,
                module,
                closure,
            });
        }
        let mut groups = HashMap::new();
        for checked in &macros {
            for id in &checked.closure.items {
                if groups.contains_key(id) {
                    continue;
                }
                let Some(indexed) = self.closure.items.get(id) else {
                    continue;
                };
                let (module, position) = (indexed.module, indexed.position);
                let items = self.closure.group_of(*id).as_ref().clone();
                groups.insert(
                    *id,
                    ClosureGroup {
                        module,
                        position,
                        items,
                    },
                );
            }
        }
        CheckSnapshot {
            modules: vec![Vec::new(); modules],
            groups,
            macros,
        }
    }
}

/// The program that checks the closures of `macros`, all of them declared in
/// `package`. `next_syntax_id` supplies identities for what is generated and
/// is advanced past them.
pub(crate) fn build_check_program(
    program: &Program,
    snapshot: &CheckSnapshot,
    package: Option<PackageId>,
    macros: &[&CheckedMacro],
    next_syntax_id: &mut usize,
) -> Program {
    let dependencies = package_dependencies(program, package);
    let wanted = macros
        .iter()
        .flat_map(|checked| checked.closure.items.iter().copied())
        .collect::<BTreeSet<_>>();
    let mut own_items: Vec<Vec<(usize, &[Item])>> = vec![Vec::new(); program.modules().len()];
    for id in &wanted {
        if let Some(group) = snapshot.groups.get(id) {
            own_items[group.module.0].push((group.position, &group.items));
        }
    }
    // Modules made while expanding come after the ones the snapshot covers.
    let expanded_of = |module: ModuleId| -> &[Item] {
        snapshot
            .modules
            .get(module.0)
            .map_or(&program.module(module).syntax.items, |items| items)
    };
    let mut items = Vec::new();
    for module in program.modules() {
        let owner = program.package_of(module.id);
        let expanded = expanded_of(module.id);
        if owner == package {
            let mut positioned = std::mem::take(&mut own_items[module.id.0]);
            positioned.sort_by_key(|(position, _)| *position);
            let mut kept = Vec::new();
            for (_, group) in positioned {
                for item in group {
                    convert_item(item, next_syntax_id, &mut kept);
                }
            }
            items.push(kept);
        } else if dependencies.contains(&owner) {
            items.push(expanded.to_vec());
        } else {
            items.push(Vec::new());
        }
    }
    // Structure and imports come from the expanded items, once every module's
    // contents are known.
    let defined = items
        .iter()
        .zip(program.modules())
        .map(|(module, source)| {
            defined_names(module.iter().chain(
                expanded_of(source.id)
                    .iter()
                    .filter(|item| matches!(item, Item::Submodule(_))),
            ))
        })
        .collect::<Vec<_>>();
    for module in program.modules() {
        let expanded = expanded_of(module.id);
        let owner = program.package_of(module.id);
        let kept = &mut items[module.id.0];
        for item in expanded {
            match item {
                Item::Submodule(submodule) => {
                    let mut submodule = submodule.clone();
                    if owner == package {
                        submodule.module.items.clear();
                        kept.push(Item::Submodule(submodule));
                    }
                }
                Item::UseDeclaration(declaration) if owner == package => {
                    let mut declaration = declaration.clone();
                    let target = program.imported_module(declaration.syntax.id);
                    if let Some(target) = target
                        && !keep_import(&mut declaration.kind, &defined[target.0])
                    {
                        continue;
                    }
                    kept.push(Item::UseDeclaration(declaration));
                }
                _ => {}
            }
        }
    }
    program.with_module_items(items, *next_syntax_id)
}

/// The packages `package` depends on, transitively, and the package itself.
fn package_dependencies(program: &Program, package: Option<PackageId>) -> HashSet<Option<PackageId>> {
    let mut found = HashSet::new();
    let Some(package) = package else {
        return found;
    };
    let Some(graph) = program.package_graph() else {
        return found;
    };
    let mut pending = vec![package];
    while let Some(next) = pending.pop() {
        for dependency in &graph.package(next).dependencies {
            if found.insert(Some(dependency.package)) {
                pending.push(dependency.package);
            }
        }
    }
    found
}

/// Keeps `item` for the check, turning macros into functions. A macro stays
/// declared as well, without its body, so names that import it still resolve.
fn convert_item(item: &Item, next_syntax_id: &mut usize, out: &mut Vec<Item>) {
    match item {
        Item::MacroDeclaration(declaration) => {
            if let Some(function) = macro_function(declaration, next_syntax_id) {
                out.push(Item::Binding(function));
            }
            let mut bodyless = declaration.clone();
            bodyless.value = None;
            out.push(Item::MacroDeclaration(bodyless));
        }
        Item::Modified(modified) => {
            let mut inner = Vec::new();
            convert_item(&modified.item, next_syntax_id, &mut inner);
            if inner.len() == 1 {
                let mut modified = modified.clone();
                modified.item = Box::new(inner.remove(0));
                out.push(Item::Modified(modified));
            } else {
                out.extend(inner);
            }
        }
        item => out.push(item.clone()),
    }
}

/// The function a macro overload with a body becomes. Its name is generated and
/// holds a character no source name can, so no code refers to it.
fn macro_function(declaration: &MacroDeclaration, next_syntax_id: &mut usize) -> Option<Binding> {
    let value = declaration.value.clone()?;
    let id = SyntaxId(*next_syntax_id);
    *next_syntax_id += 1;
    let mut syntax: Syntax = declaration.syntax.clone();
    syntax.id = id;
    Some(Binding {
        syntax,
        docs: Vec::new(),
        member_modifiers: Vec::new(),
        visibility: Visibility::Private,
        kind: BindingKind::Def,
        mutable: false,
        signal: false,
        name: format!("macro#{}#{}", declaration.name, declaration.syntax.id.0),
        annotation: declaration.annotation.clone(),
        companion_target: None,
        companion_generics: None,
        track_caller: false,
        value: Some(value),
    })
}

/// The names `items` define at module level.
fn defined_names<'a>(items: impl Iterator<Item = &'a Item>) -> HashSet<String> {
    let mut names = HashSet::new();
    for item in items {
        names.extend(item_kind(item).2);
        if let Item::Submodule(submodule) = item {
            names.insert(submodule.name.clone());
        }
    }
    names
}

/// Drops the names of an import that nothing defines. `false` when none remain.
fn keep_import(kind: &mut UseKind, defined: &HashSet<String>) -> bool {
    match kind {
        UseKind::Selected(selected) => {
            selected.retain(|SelectedImport { name, .. }| defined.contains(name));
            !selected.is_empty()
        }
        UseKind::Renamed { item, .. } => defined.contains(item),
        _ => true,
    }
}

/// Whether `inner` lies within `outer`, in the same source.
fn span_within(inner: &Span, outer: &Span) -> bool {
    match (inner, outer) {
        (
            Span::User {
                source: inner_source,
                range: inner,
                ..
            },
            Span::User {
                source: outer_source,
                range: outer,
                ..
            },
        ) => inner_source == outer_source && outer.start <= inner.start && inner.end <= outer.end,
        _ => false,
    }
}

/// Checks the compile-time code of the closures `analysis` recorded: each
/// package's closures are resolved and type-checked as one program. Every
/// diagnostic names the macro whose closure contained the item it is in.
pub(crate) fn check_closures(program: &Program, analysis: &mut MacroAnalysis) -> Vec<Diagnostic> {
    let Some(snapshot) = analysis.check.take() else {
        return Vec::new();
    };
    let mut next_syntax_id = analysis.next_syntax_id;
    let mut packages: Vec<Option<PackageId>> = Vec::new();
    for checked in &snapshot.macros {
        let package = program.package_of(checked.module);
        if !packages.contains(&package) {
            packages.push(package);
        }
    }
    let mut diagnostics = Vec::new();
    for package in packages {
        let macros = snapshot
            .macros
            .iter()
            .filter(|checked| program.package_of(checked.module) == package)
            .collect::<Vec<_>>();
        let check = build_check_program(program, &snapshot, package, &macros, &mut next_syntax_id);
        // A package that holds only closure items does not meet the contract
        // the standard library has with the compiler, which is no finding.
        let partial = |diagnostic: &Diagnostic| {
            matches!(diagnostic.span, Span::Compiler)
                && (diagnostic.message.starts_with("standard library")
                    || diagnostic.message.starts_with("compiler-provided macro"))
        };
        let (resolved, mut reported) =
            NameResolver::pre_expanded(next_syntax_id).resolve_program_recovering(check);
        reported.retain(|diagnostic| !partial(diagnostic));
        if reported.is_empty()
            && let Some(resolved) = resolved
            && let Err(found) = TypeChecker::new().check(resolved)
        {
            reported = found;
            reported.retain(|diagnostic| !partial(diagnostic));
        }
        let reported = Some(reported);
        for mut diagnostic in reported.unwrap_or_default() {
            let owner = macros.iter().find(|checked| {
                checked.closure.items.iter().any(|id| {
                    snapshot.groups.get(id).is_some_and(|group| {
                        group
                            .items
                            .iter()
                            .any(|item| span_within(&diagnostic.span, &item.syntax().span))
                    })
                })
            });
            if let Some(owner) = owner {
                diagnostic.message.push_str(&format!(
                    "\n  note: while checking the compile-time code of macro `{}`",
                    owner.name
                ));
            }
            diagnostics.push(diagnostic);
        }
    }
    diagnostics
}
