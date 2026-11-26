//! Builds the program that checks macro code as ordinary code.
//!
//! The program mirrors what compiling a package's macros needs: every
//! dependency package whole, after expansion and including its compile-time
//! items, and from the package being expanded only the items in the closures
//! being checked. A name a closure misses is then a resolution error.

use std::collections::{BTreeSet, HashMap, HashSet};

use staple_project::PackageId;
use staple_syntax::{
    Binding, BindingKind, Diagnostic, Expression, Item, MacroDeclaration, Pattern, SelectedImport, Span, Syntax,
    SyntaxId, UseKind, Visibility,
};

use super::closure::{ItemId, MacroClosure, item_kind};
use super::compiled_call::{PackageLibraryBuild, PackageLibraryMacro};
use super::lower_quote::{QuoteLowering, TemplateTable};
use super::{MacroAnalysis, MacroDefinition, MacroExpander, MacroKind};
use crate::{Lowerer, ModuleId, NameResolver, Program, TypeChecker};

/// Which macros have their closures checked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckMode {
    /// The macros that were invoked.
    Invoked,
    /// Every macro declared in the program.
    All,
    /// Every macro declared in the program, with the helper and macro bodies
    /// expanded because the program becomes a package library's macro-mode
    /// object. Not selected by the environment variable.
    PackageLibrary,
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
    /// What each compile-time helper and macro declaration expanded to.
    pub(crate) compile_time: HashMap<ItemId, Vec<Item>>,
    pub(crate) macros: Vec<CheckedMacro>,
    /// Items the caller adds to their modules after the program is built,
    /// such as the `@compiled_only` items of a closure library. Imports of
    /// their names are kept.
    pub(crate) added: Vec<(ModuleId, Item)>,
}

impl MacroExpander {
    /// Expands the macro calls in the bodies of `Parse` implementations, which
    /// the interpreter runs as written and expansion otherwise leaves alone.
    pub(super) fn expand_compile_time_items(&mut self, module: ModuleId, items: &mut [Item]) {
        for item in items {
            self.expand_compile_time_item(module, item);
        }
    }

    /// Expands a compile-time helper's body, which normal expansion leaves as
    /// written because the interpreter evaluates it on demand. A check program
    /// keeps the helper as code, so its macro calls must be expanded first.
    pub(super) fn expand_compile_time_helper(&mut self, module: ModuleId, item: &mut Item) {
        if let Item::Modified(modified) = item {
            return self.expand_compile_time_helper(module, &mut modified.item);
        }
        if !matches!(item, Item::Binding(binding) if super::binding_is_compile_time_helper(binding)) {
            return;
        }
        self.expanded_check_items.insert(item.syntax().id);
        let saved = self.save_state();
        self.compile_time_depth += 1;
        self.expand_block_item_contents(module, item, 0);
        self.compile_time_depth -= 1;
        self.restore_state(saved);
    }

    /// Expands a macro declaration's body for a check program, which keeps it
    /// as the function a macro call runs rather than dropping it.
    pub(super) fn expand_macro_declaration_for_check(&mut self, module: ModuleId, item: &mut Item) {
        if let Item::Modified(modified) = item {
            return self.expand_macro_declaration_for_check(module, &mut modified.item);
        }
        let Item::MacroDeclaration(declaration) = item else {
            return;
        };
        let Some(key) = self.macro_by_item.get(&declaration.syntax.id).cloned() else {
            return;
        };
        let Some(definition) = self.definitions.get(&key) else {
            return;
        };
        if !matches!(definition.kind, MacroKind::User(_)) {
            return;
        }
        let Some(value) = definition.declaration.value.clone() else {
            return;
        };
        self.expanded_check_items.insert(declaration.syntax.id);
        let saved = self.save_state();
        self.compile_time_depth += 1;
        let value = self.expand_expression(module, value, 0);
        self.compile_time_depth -= 1;
        self.restore_state(saved);
        if let Some(definition) = self.definitions.get_mut(&key) {
            definition.declaration.value = Some(value.clone());
            definition.kind = MacroKind::User(value.clone());
        }
        declaration.value = Some(value);
    }

    fn expand_compile_time_item(&mut self, module: ModuleId, item: &mut Item) {
        if let Item::Modified(modified) = item {
            return self.expand_compile_time_item(module, &mut modified.item);
        }
        if !self.is_compile_time_parse_item(module, item) {
            return;
        }
        if let Item::TraitImplementation(implementation) = item {
            let saved = self.save_state();
            self.compile_time_depth += 1;
            for member in &mut implementation.members {
                member.value = self.expand_expression(module, member.value.clone(), 0);
            }
            self.compile_time_depth -= 1;
            self.restore_state(saved);
        }
    }

    /// Collects the closures to check and the items they contain. Runs after
    /// the module walk, before the index is released.
    pub(super) fn check_snapshot(
        &mut self,
        mode: CheckMode,
        own_package: Option<PackageId>,
        modules: usize,
    ) -> CheckSnapshot {
        // What the check itself expands below does not count as invoked.
        let invoked = self.invoked_macros.clone();
        let wanted = |definition: &MacroDefinition| {
            mode != CheckMode::Invoked || invoked.contains(&definition.key)
        };
        // The standard library is checked whole even as the program's own
        // package, as `build_check_program` does.
        let own_is_whole = own_package.is_some()
            && self.syntax_module.map(|module| self.module_packages[module.0]) == Some(own_package);
        let mut declared = self
            .definitions
            .values()
            .filter(|definition| matches!(definition.kind, MacroKind::User(_)))
            .cloned()
            .collect::<Vec<_>>();
        declared.sort_by_key(|definition| definition.declaration.syntax.id);
        for definition in &declared {
            // Every check program holds the other packages whole, so all of
            // their macros need their closures expanded. Of the program's own
            // package, it holds only the closures being checked.
            if self.module_packages[definition.key.module.0] == own_package
                && !own_is_whole
                && !wanted(definition)
            {
                continue;
            }
            // Making ready a macro nothing called must not fail the build with
            // a cycle that a build without the check would not report.
            let soft = usize::from(!wanted(definition));
            self.soft_stage += soft;
            let span = definition.declaration.syntax.span.clone();
            let _ = self.ensure_ready(definition, &span);
            self.soft_stage -= soft;
        }
        let mut macros = Vec::new();
        for definition in declared.iter().filter(|definition| wanted(definition)) {
            let start = definition.declaration.syntax.id;
            let closure = match self.macro_closures.get(&start) {
                Some(closure) => closure.clone(),
                // Templates need nothing to run, so nothing computed theirs.
                None => self.macro_closure(start),
            };
            macros.push(CheckedMacro {
                name: definition.key.name.clone(),
                module: definition.key.module,
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
                let mut items = self.closure.group_of(*id).as_ref().clone();
                self.expand_compile_time_items(module, &mut items);
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
        // The module walk leaves compile-time helpers and macro declarations as
        // written; what they expand to is kept with the index.
        let mut pending = Vec::new();
        for (id, indexed) in &self.closure.items {
            if let Some(group) = &indexed.group
                && group.iter().any(|item| {
                    matches!(item, Item::MacroDeclaration(_))
                        || matches!(item, Item::Binding(binding)
                            if super::binding_is_compile_time_helper(binding))
                })
            {
                pending.push((*id, indexed.module, group.as_ref().clone()));
            }
        }
        let mut compile_time = HashMap::new();
        for (id, module, mut items) in pending {
            self.expand_compile_time_items(module, &mut items);
            compile_time.insert(id, items);
        }
        CheckSnapshot {
            modules: vec![Vec::new(); modules],
            groups,
            compile_time,
            macros,
            added: Vec::new(),
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
) -> (Program, TemplateTable) {
    build_check_program_scoped(
        program,
        snapshot,
        package,
        macros,
        next_syntax_id,
        true,
        false,
    )
}

/// Like [`build_check_program`], with a choice between whole dependency
/// packages (the closure check) and only the closure items of every package
/// (the harness's small executable programs), and between a closure-only own
/// package and the whole of it (a package library).
pub(crate) fn build_check_program_with(
    program: &Program,
    snapshot: &CheckSnapshot,
    package: Option<PackageId>,
    macros: &[&CheckedMacro],
    next_syntax_id: &mut usize,
    whole_dependencies: bool,
) -> (Program, TemplateTable) {
    build_check_program_scoped(
        program,
        snapshot,
        package,
        macros,
        next_syntax_id,
        whole_dependencies,
        false,
    )
}

fn build_check_program_scoped(
    program: &Program,
    snapshot: &CheckSnapshot,
    package: Option<PackageId>,
    macros: &[&CheckedMacro],
    next_syntax_id: &mut usize,
    whole_dependencies: bool,
    whole_own_package: bool,
) -> (Program, TemplateTable) {
    let syntax_module = program.standard_library_syntax();
    let dependencies = package_dependencies(program, package);
    // The program's own package is checked by its closures. A dependency's
    // macros are built with the dependency, so its package is checked whole.
    // The standard library is always a dependency in a real build, even when a
    // corpus input is a standard-library file, so it is checked whole too.
    let standard_library_package = syntax_module.and_then(|module| program.package_of(module));
    let whole = whole_dependencies
        && (whole_own_package
            || package != program.package_of(program.entry())
            || (package.is_some() && package == standard_library_package));
    // Dependencies may be kept whole or reduced to the closure items the
    // checked macros reach. The checked package can be whole while its
    // dependencies are only closures: a package library checks every item of
    // its own package but only needs the dependency code those items reach.
    let closure_only = !whole_dependencies;
    let own_closure_only = !whole_own_package && !whole;
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
        let closure_module = (closure_only && owner != package)
            || (owner == package && own_closure_only);
        if closure_module {
            let mut positioned = std::mem::take(&mut own_items[module.id.0]);
            positioned.sort_by_key(|(position, _)| *position);
            let mut kept = Vec::new();
            for (_, group) in positioned {
                for item in group {
                    convert_item(item, next_syntax_id, syntax_module, &mut kept);
                }
            }
            items.push(kept);
        } else if owner == package || dependencies.contains(&owner) {
            let mut kept = Vec::new();
            for item in expanded {
                if owner == package {
                    convert_item(item, next_syntax_id, syntax_module, &mut kept);
                } else {
                    kept.push(item.clone());
                }
            }
            items.push(kept);
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
            let added = snapshot
                .added
                .iter()
                .filter(|(owner, _)| *owner == source.id)
                .map(|(_, item)| item);
            defined_names(
                module
                    .iter()
                    .chain(expanded_of(source.id).iter().filter(
                        |item| matches!(item, Item::Submodule(submodule) if !submodule.companion),
                    ))
                    .chain(added),
            )
        })
        .collect::<Vec<_>>();
    let module_names = program
        .modules()
        .iter()
        .filter_map(|module| {
            program
                .module_dotted_name(module.id)
                .map(|name| (name, module.id))
        })
        .collect::<Vec<_>>();
    for module in program.modules() {
        let expanded = expanded_of(module.id);
        let owner = program.package_of(module.id);
        let closure_module = (closure_only && owner != package)
            || (owner == package && own_closure_only);
        let kept = &mut items[module.id.0];
        for item in expanded {
            match item {
                Item::Submodule(submodule) => {
                    let mut submodule = submodule.clone();
                    if closure_module {
                        submodule.module.items.clear();
                        kept.push(Item::Submodule(submodule));
                    }
                }
                Item::UseDeclaration(declaration) if closure_module => {
                    let mut declaration = declaration.clone();
                    let target = check_import_target(program, module.id, &declaration, &module_names);
                    if let Some(target) = target {
                        // A companion's names may be defined in any of its
                        // blocks, while an import names its namespace.
                        let blocks = program.companion_blocks(target);
                        let target_defined = match blocks.as_slice() {
                            [only] => std::borrow::Cow::Borrowed(&defined[only.0]),
                            _ => std::borrow::Cow::Owned(
                                blocks
                                    .iter()
                                    .flat_map(|block| defined[block.0].iter().cloned())
                                    .collect::<HashSet<_>>(),
                            ),
                        };
                        let keep = match (&declaration.kind, declaration.path.last()) {
                            (UseKind::Dotted, Some(last))
                                if program.module_dotted_name(target).as_deref()
                                    != Some(dotted_import_path(program, module.id, &declaration).as_str()) =>
                            {
                                target_defined.contains(last)
                            }
                            _ => keep_import(&mut declaration.kind, &target_defined),
                        };
                        if !keep {
                            continue;
                        }
                    }
                    kept.push(Item::UseDeclaration(declaration));
                }
                _ => {}
            }
        }
    }
    let mut table = TemplateTable::default();
    if let Some(syntax_module) = program.standard_library_syntax() {
        for (module, module_items) in items.iter_mut().enumerate() {
            let mut lowering = QuoteLowering {
                syntax_module,
                module: ModuleId(module),
                next_syntax_id,
                table: &mut table,
            };
            for item in module_items {
                lowering.lower_item(item);
            }
        }
    }
    let mut check_program = program.with_module_items(items, *next_syntax_id);
    if closure_only {
        // The structure comes from the program as it was before expansion, so
        // a module a macro generated, such as the `companion` block
        // `typegroup` writes, is only an item in its group. Register it as
        // expansion does.
        check_program.register_generated_inline_modules(next_syntax_id);
    }
    (check_program, table)
}

/// The packages `package` depends on, transitively, and the package itself.
fn package_dependencies(program: &Program, package: Option<PackageId>) -> HashSet<Option<PackageId>> {
    let mut found = HashSet::new();
    let Some(package) = package else {
        // A single file has no package of its own; every package is one of its
        // dependencies.
        found.extend(
            program
                .modules()
                .iter()
                .map(|module| program.package_of(module.id))
                .filter(Option::is_some),
        );
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
fn convert_item(
    item: &Item,
    next_syntax_id: &mut usize,
    syntax_module: Option<ModuleId>,
    out: &mut Vec<Item>,
) {
    match item {
        Item::MacroDeclaration(declaration) => {
            if let Some(function) = macro_function(declaration, next_syntax_id, syntax_module) {
                out.push(Item::Binding(function));
                if let Some(syntax_module) = syntax_module
                    && let Some(entry) = entry_function(declaration, next_syntax_id, syntax_module)
                {
                    out.push(Item::Binding(entry));
                }
            }
            let mut bodyless = declaration.clone();
            bodyless.value = None;
            out.push(Item::MacroDeclaration(bodyless));
        }
        Item::Modified(modified) => {
            let mut inner = Vec::new();
            convert_item(&modified.item, next_syntax_id, syntax_module, &mut inner);
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
fn macro_function(
    declaration: &MacroDeclaration,
    next_syntax_id: &mut usize,
    syntax_module: Option<ModuleId>,
) -> Option<Binding> {
    let mut value = declaration.value.clone()?;
    let slots = match &declaration.annotation {
        Some(annotation) => slot_count(annotation),
        // Without a signature, every function the value nests is a slot.
        None => Some(nested_functions(&value).len()),
    };
    if let Some(slots) = slots
        && slots > 1
    {
        merge_slots(&mut value, slots, next_syntax_id);
    }
    if let Some(syntax_module) = syntax_module
        && declaration
            .annotation
            .as_ref()
            .is_some_and(|annotation| super::type_contains_named(annotation, "MacroCallMetadata"))
    {
        name_metadata_pattern(&mut value, syntax_module, next_syntax_id);
    }
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
        name: macro_function_name(declaration),
        annotation: declaration.annotation.clone(),
        companion_target: None,
        companion_generics: None,
        track_caller: false,
        value: Some(value),
    })
}

/// The name of the function a macro overload becomes.
pub(crate) fn macro_function_name(declaration: &MacroDeclaration) -> String {
    format!("macro#{}#{}", declaration.name, declaration.syntax.id.0)
}

/// The name of a macro overload's entry function.
pub(crate) fn entry_function_name(declaration: &MacroDeclaration) -> String {
    format!("entry#{}#{}", declaration.name, declaration.syntax.id.0)
}

/// The parameter types of a macro signature, one per argument the macro takes:
/// each slot of a slotted product, none for `()`, and otherwise the one type.
fn signature_parameters(parameter: &staple_syntax::Type) -> Vec<&staple_syntax::Type> {
    match parameter {
        staple_syntax::Type::Product(product) if product.slotted => {
            product.elements.iter().map(|element| &element.ty).collect()
        }
        staple_syntax::Type::Product(product) if product.elements.is_empty() => Vec::new(),
        parameter => vec![parameter],
    }
}

/// The functions a macro's value nests, outermost first. A macro written
/// `(a; b) => body` is parsed one function per slot, and without a signature
/// each of them is a parameter.
fn nested_functions(value: &Expression) -> Vec<&staple_syntax::FunctionExpression> {
    let mut functions = Vec::new();
    let mut current = value;
    while let Expression::Function(function) = current {
        functions.push(function.as_ref());
        current = &function.body;
    }
    functions
}

/// The parameter types of a macro without a signature, one per slot, when
/// every slot is typed. A slot without a type has nothing to decode. Slots are
/// nested functions, or the elements of one function's slotted product.
fn lambda_parameters(value: &Expression) -> Option<Vec<staple_syntax::Type>> {
    fn typed(pattern: &Pattern) -> Option<staple_syntax::Type> {
        match pattern {
            Pattern::Product(product) if product.annotation.is_none() => {
                product.elements.iter().try_for_each(|element| typed(element).map(drop))?;
                Some(pattern.ty())
            }
            Pattern::Binding(_) | Pattern::Wildcard(_) | Pattern::At(_) | Pattern::Product(_) => {
                match pattern.ty() {
                    staple_syntax::Type::Inferred(_) => None,
                    ty => Some(ty),
                }
            }
            _ => None,
        }
    }
    match nested_functions(value).as_slice() {
        [] => None,
        // One function can also take every slot as a slotted product.
        [function] => match &function.pattern {
            Pattern::Product(product) if product.slotted => product.elements.iter().map(typed).collect(),
            pattern => Some(vec![typed(pattern)?]),
        },
        functions => functions.iter().map(|function| typed(&function.pattern)).collect(),
    }
}

/// Builds the generated code of an entry function, with fresh syntax ids.
struct EntryBuilder<'a> {
    like: Syntax,
    syntax_module: ModuleId,
    next_syntax_id: &'a mut usize,
}

impl EntryBuilder<'_> {
    fn syntax(&mut self) -> Syntax {
        let mut syntax = self.like.clone();
        syntax.id = SyntaxId(*self.next_syntax_id);
        *self.next_syntax_id += 1;
        syntax
    }

    /// A name that resolves in `std.syntax`, wherever the macro is.
    fn library_syntax(&mut self) -> Syntax {
        let mut syntax = self.syntax();
        syntax.definition_module = Some(self.syntax_module.0);
        syntax
    }

    fn library_name(&mut self, name: &str) -> Expression {
        Expression::Name(staple_syntax::NameExpression {
            syntax: self.library_syntax(),
            name: name.to_owned(),
        })
    }

    fn library_type(&mut self, name: &str) -> staple_syntax::Type {
        staple_syntax::Type::Named(staple_syntax::NamedType {
            syntax: self.library_syntax(),
            namespace: None,
            name: name.to_owned(),
        })
    }

    fn member(&mut self, trait_: &str, member: &str) -> Expression {
        let value = self.library_name(trait_);
        Expression::Access(staple_syntax::AccessExpression {
            syntax: self.syntax(),
            value: Box::new(value),
            accessor: staple_syntax::Accessor::Name(member.to_owned()),
        })
    }

    fn call(&mut self, callee: Expression, argument: Expression) -> Expression {
        Expression::Call(staple_syntax::CallExpression {
            spread: false,
            syntax: self.syntax(),
            callee: Box::new(callee),
            argument: Box::new(argument),
        })
    }

    fn local(&mut self, name: &str) -> Expression {
        Expression::Name(staple_syntax::NameExpression {
            syntax: self.syntax(),
            name: name.to_owned(),
        })
    }

    /// `let name = value`.
    fn bind(&mut self, name: &str, value: Expression) -> Item {
        Item::Binding(Binding {
            syntax: self.syntax(),
            docs: Vec::new(),
            member_modifiers: Vec::new(),
            visibility: Visibility::Private,
            kind: BindingKind::Let,
            mutable: false,
            signal: false,
            name: name.to_owned(),
            annotation: None,
            companion_target: None,
            companion_generics: None,
            track_caller: false,
            value: Some(value),
        })
    }

    fn product(&mut self, slotted: bool, elements: Vec<Expression>) -> Expression {
        let elements = elements
            .into_iter()
            .map(|value| staple_syntax::ProductElement {
                syntax: self.syntax(),
                name: None,
                designated: false,
                value,
                spread: false,
                named_spread: false,
            })
            .collect();
        Expression::Product(staple_syntax::ProductExpression {
            slotted,
            syntax: self.syntax(),
            elements,
        })
    }

    /// `FromMatch.from_match.<parameter> (__match_child (arguments, index))`.
    fn decode(&mut self, parameter: staple_syntax::Type, index: usize) -> Expression {
        let member = self.member("FromMatch", "from_match");
        let decoder = Expression::TypeApplication(Box::new(staple_syntax::TypeApplicationExpression {
            syntax: self.syntax(),
            value: Box::new(member),
            arguments: vec![parameter],
        }));
        let child = self.library_name("__match_child");
        let arguments = self.local("arguments");
        let index = Expression::Integer(staple_syntax::IntegerExpression {
            syntax: self.syntax(),
            literal: index.to_string(),
        });
        let pair = self.product(false, vec![arguments, index]);
        let node = self.call(child, pair);
        self.call(decoder, node)
    }
}

/// The entry function of a macro overload with a body and a signature.
///
/// The parent calls it with the call's match, a list with one node per
/// parameter. It decodes each parameter with `FromMatch`, calls the macro's
/// function and splices the result, so a macro whose parameters can't be
/// decoded or whose result can't be spliced is a type error here. Each value
/// is bound before it is passed, so a parameter that borrows a product gets a
/// place to borrow from.
fn entry_function(
    declaration: &MacroDeclaration,
    next_syntax_id: &mut usize,
    syntax_module: ModuleId,
) -> Option<Binding> {
    let parameters = match (declaration.annotation.as_ref(), declaration.value.as_ref()) {
        (Some(staple_syntax::Type::Function(signature)), _) => {
            signature_parameters(&signature.parameter).into_iter().cloned().collect()
        }
        (None, Some(value)) => lambda_parameters(value)?,
        _ => return None,
    };
    let mut builder = EntryBuilder {
        like: declaration.syntax.clone(),
        syntax_module,
        next_syntax_id,
    };
    let mut items = Vec::new();
    let mut decoded = Vec::new();
    for (index, parameter) in parameters.iter().enumerate() {
        // The type is read again from its own tokens, so it has fresh ids and
        // resolves where the macro wrote it.
        let parameter = staple_syntax::parse_type_fragment(
            parameter.syntax(),
            false,
            builder.next_syntax_id,
        )
        .ok()?;
        let value = builder.decode(parameter, index);
        let name = format!("parameter{index}");
        items.push(builder.bind(&name, value));
        decoded.push(builder.local(&name));
    }
    let function = Expression::Name(staple_syntax::NameExpression {
        syntax: builder.syntax(),
        name: macro_function_name(declaration),
    });
    let call = match decoded.len() {
        0 => {
            let unit = builder.product(false, Vec::new());
            builder.call(function, unit)
        }
        1 => {
            let argument = decoded.remove(0);
            builder.call(function, argument)
        }
        _ => {
            let slots = builder.product(true, decoded);
            match builder.call(function, slots) {
                Expression::Call(mut call) => {
                    call.spread = true;
                    Expression::Call(call)
                }
                call => call,
            }
        }
    };
    items.push(builder.bind("result", call));
    let splice = builder.member("Splice", "splice");
    let result = builder.local("result");
    items.push(Item::Expression(builder.call(splice, result)));
    let body = Expression::Block(staple_syntax::BlockExpression {
        syntax: builder.syntax(),
        items,
    });
    let pattern = Pattern::Binding(staple_syntax::BindingPattern {
        syntax: builder.syntax(),
        mutable: false,
        moved: false,
        name: "arguments".to_owned(),
        resolution_name: None,
        ty: staple_syntax::Type::Inferred(staple_syntax::InferredType::default()),
    });
    // A signature makes it a function of its own rather than a template that
    // is only instantiated where it is used.
    let parameter_type = builder.library_type("MatchNode");
    let result_type = builder.library_type("Syntax");
    let effects = staple_syntax::EffectSet {
        syntax: builder.syntax(),
        variable: None,
        resources: Vec::new(),
        state: Vec::new(),
    };
    let annotation = staple_syntax::Type::Function(staple_syntax::FunctionType {
        syntax: builder.syntax(),
        parameter: Box::new(parameter_type),
        mutable_marker: None,
        moved_marker: None,
        effects,
        result: Box::new(result_type),
    });
    let value = Expression::Function(Box::new(staple_syntax::FunctionExpression {
        syntax: builder.syntax(),
        generics: None,
        pattern,
        effect_lower_bound: None,
        body: Box::new(body),
    }));
    Some(Binding {
        syntax: builder.syntax(),
        docs: Vec::new(),
        member_modifiers: Vec::new(),
        visibility: Visibility::Private,
        kind: BindingKind::Def,
        mutable: false,
        signal: false,
        name: entry_function_name(declaration),
        annotation: Some(annotation),
        companion_target: None,
        companion_generics: None,
        track_caller: false,
        value: Some(value),
    })
}

/// The names `items` define at module level, as items or submodules.
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

/// The module an import names in the check program. Imports rooted at
/// `package` name the importing package's own modules, which the loader does
/// not resolve through `Program::imported_module`; matching the dotted module
/// names covers those and any other package-rooted path.
fn dotted_import_path(
    program: &Program,
    source: ModuleId,
    declaration: &staple_syntax::UseDeclaration,
) -> String {
    let mut parts = declaration.path.clone();
    if parts.first().is_some_and(|part| part == "package")
        && let Some(package) = program.package_of(source)
        && let Some(graph) = program.package_graph()
    {
        parts[0] = graph.package(package).name.clone();
    }
    parts.join(".")
}

fn check_import_target(
    program: &Program,
    source: ModuleId,
    declaration: &staple_syntax::UseDeclaration,
    module_names: &[(String, ModuleId)],
) -> Option<ModuleId> {
    if let Some(target) = program.imported_module(declaration.syntax.id) {
        return Some(target);
    }
    let mut parts = declaration.path.clone();
    if parts.first().is_some_and(|part| part == "package") {
        let package = program.package_of(source)?;
        let graph = program.package_graph()?;
        parts[0] = graph.package(package).name.clone();
    }
    let mut candidate = parts.join(".");
    loop {
        if let Some((_, target)) = module_names.iter().find(|(name, _)| name == &candidate) {
            return Some(*target);
        }
        let (prefix, _) = candidate.rsplit_once('.')?;
        candidate = prefix.to_owned();
    }
}

/// Drops the names of an import that nothing defines. `false` when none remain.
fn keep_import(kind: &mut UseKind, defined: &HashSet<String>) -> bool {
    match kind {
        UseKind::Selected(selected) => {
            selected.retain(|SelectedImport { name, .. }| defined.contains(name));
            !selected.is_empty()
        }
        UseKind::Renamed { item, .. } => defined.contains(item),
        // A namespace import is only useful when the module it names is part
        // of the check program.
        _ => !defined.is_empty(),
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
        ) => {
            (inner_source == outer_source || inner_source.is_none() || outer_source.is_none())
                && outer.start <= inner.start
                && inner.end <= outer.end
        }
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
    let mut diagnostics = Vec::new();
    for (_package, macros, check, _templates) in
        check_programs(program, &snapshot, &mut next_syntax_id)
    {
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
        if reported.is_empty() {
            match resolved {
                Some(resolved) => {
                    if let Err(found) = TypeChecker::new().check(resolved) {
                        reported = found;
                        reported.retain(|diagnostic| !partial(diagnostic));
                    }
                }
                // The resolver gave up without saying why, which is not a pass.
                None => reported.push(Diagnostic::new(
                    Span::Compiler,
                    "the check of macro code could not resolve its program",
                )),
            }
        }
        for mut diagnostic in reported {
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

/// The check program of each package that has checked macros, in the order the
/// packages first appear. The macros come from `snapshot`; `next_syntax_id` is
/// advanced past every generated identity.
pub(crate) fn check_programs<'a>(
    program: &Program,
    snapshot: &'a CheckSnapshot,
    next_syntax_id: &mut usize,
) -> Vec<(Option<PackageId>, Vec<&'a CheckedMacro>, Program, TemplateTable)> {
    let mut packages: Vec<Option<PackageId>> = Vec::new();
    for checked in &snapshot.macros {
        let package = program.package_of(checked.module);
        if !packages.contains(&package) {
            packages.push(package);
        }
    }
    packages
        .into_iter()
        .map(|package| {
            let macros = snapshot
                .macros
                .iter()
                .filter(|checked| program.package_of(checked.module) == package)
                .collect::<Vec<_>>();
            let (check, templates) =
                build_check_program(program, snapshot, package, &macros, next_syntax_id);
            (package, macros, check, templates)
        })
        .collect()
}

/// Expands `program` and builds the macro-mode form of `package`: every macro
/// of the package becomes a function with an entry, its dependencies stay
/// whole, and its compile-time items are kept, so the result can be lowered
/// into the package's macro-mode object and linked into its package library.
pub fn prepare_package_library(
    program: Program,
    package: PackageId,
) -> Result<PackageLibraryBuild, Vec<Diagnostic>> {
    let (expanded, analysis, diagnostics) =
        super::expand_program_checked(program, false, Some(CheckMode::PackageLibrary), false)?;
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let Some(snapshot) = analysis.check else {
        return Err(vec![Diagnostic::new(
            Span::Compiler,
            "internal error: expanding a package library kept no check snapshot",
        )]);
    };
    let macros = snapshot
        .macros
        .iter()
        .filter(|checked| expanded.package_of(checked.module) == Some(package))
        .collect::<Vec<_>>();
    let mut next_syntax_id = analysis.next_syntax_id;
    let (check, mut templates) = build_check_program_scoped(
        &expanded,
        &snapshot,
        Some(package),
        &macros,
        &mut next_syntax_id,
        true,
        true,
    );
    // Dependency templates belong to the dependencies' own libraries, which
    // this library links.
    templates
        .templates
        .retain(|template| expanded.package_of(template.module) == Some(package));
    let next_syntax_id = check.next_syntax_id();
    let resolved = NameResolver::pre_expanded(next_syntax_id).resolve_program(check)?;
    let typed = TypeChecker::new().check(resolved)?;
    let lowered = Lowerer::new().lower(&typed)?;

    let mut found = Vec::new();
    for module in expanded.modules() {
        if expanded.package_of(module.id) != Some(package) {
            continue;
        }
        for item in &module.syntax.items {
            package_library_macros(item, module.id, &lowered, &mut found);
        }
    }
    found.sort_by_key(|macros| (macros.module.0, macros.declaration.0));

    Ok(PackageLibraryBuild {
        expanded,
        lowered,
        macros: found,
        templates,
    })
}

/// Collects the macro declarations of one item, and of the items it wraps.
fn package_library_macros(
    item: &Item,
    module: ModuleId,
    lowered: &crate::LoweredModule,
    out: &mut Vec<PackageLibraryMacro>,
) {
    match item {
        Item::MacroDeclaration(declaration) => {
            let entry_name = entry_function_name(declaration);
            if let Some(entry_symbol) = crate::macro_runtime::instance_symbol(lowered, &entry_name) {
                out.push(PackageLibraryMacro {
                    module,
                    declaration: declaration.syntax.id,
                    name: declaration.name.clone(),
                    span: declaration.syntax.span.clone(),
                    entry_symbol,
                });
            }
        }
        Item::Modified(modified) => package_library_macros(&modified.item, module, lowered, out),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ProgramLoader;

    struct Built {
        program: Program,
        templates: TemplateTable,
    }

    /// Expands `source` and builds the check program of its macros' closures.
    fn build(source: &str, mode: CheckMode) -> Built {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let source = format!("use std.syntax.(parse_quote, quote, Expr, Item, Sequence, Syntax)\n{source}");
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, &root)
            .expect("source should load");
        let (program, analysis, _) =
            super::super::expand_program_checked(program, true, Some(mode), false)
                .expect("source should expand");
        let snapshot = analysis.check.expect("the snapshot is kept");
        let package = program.package_of(program.entry());
        let macros = snapshot
            .macros
            .iter()
            .filter(|checked| program.package_of(checked.module) == package)
            .collect::<Vec<_>>();
        let mut next = analysis.next_syntax_id;
        let (program, templates) = build_check_program(&program, &snapshot, package, &macros, &mut next);
        Built { program, templates }
    }

    const HELPER_MACRO: &str = concat!(
        "use std.clone.Clone\n",
            "def keep = value: Expr => Expr :: Clone.clone value\n",
        "macro same = value: Expr => keep value\n",
        "let a = same 1\n",
    );

    fn entry_items(built: &Built) -> &[Item] {
        &built.program.module(built.program.entry()).syntax.items
    }

    fn function_names(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Binding(binding) if binding.kind == BindingKind::Def => {
                    Some(binding.name.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_macro_becomes_a_function_with_a_reserved_name() {
        let built = build(HELPER_MACRO, CheckMode::Invoked);
        let items = entry_items(&built);
        let functions = function_names(items);
        assert!(
            functions.iter().any(|name| name.starts_with("macro#same#")),
            "{functions:?}"
        );
        // The declaration stays, without its body, so imports of it resolve.
        assert!(items.iter().any(|item| matches!(item,
            Item::MacroDeclaration(declaration) if declaration.name == "same" && declaration.value.is_none())));
    }

    #[test]
    fn a_macro_with_a_signature_gets_an_entry_function() {
        let built = build(
            concat!(
                "macro pair: (Ident; Option<Expr>) -> Expr = (name; value) => name\n",
                "macro bare = value: Expr => value\n",
                "let a = pair x\nlet b = bare 1\n",
            ),
            CheckMode::Invoked,
        );
        let functions = function_names(entry_items(&built));
        assert!(functions.iter().any(|name| name.starts_with("entry#pair#")), "{functions:?}");
        // A parameter typed on itself gets one too.
        assert!(functions.iter().any(|name| name.starts_with("entry#bare#")), "{functions:?}");
        let entry = entry_items(&built)
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name.starts_with("entry#pair#") => Some(binding),
                _ => None,
            })
            .expect("the entry function is kept");
        let rendered = format!("{:?}", entry.value);
        assert_eq!(rendered.matches("\"from_match\"").count(), 2, "{rendered}");
        assert!(rendered.contains("spread: true"), "two slots are spread: {rendered}");
        assert!(rendered.contains("\"splice\""), "{rendered}");
    }

    #[test]
    fn overloads_become_one_function_each() {
        let built = build(
            concat!(
                "use std.clone.Clone\n",
            "def keep = value: Expr => Expr :: Clone.clone value\n",
                "macro pick = move value: Expr => keep value\n",
                "macro pick = (move a: Expr; b: Expr) => keep a\n",
                "let x = pick 1\nlet y = pick 1 2\n",
            ),
            CheckMode::Invoked,
        );
        let count = function_names(entry_items(&built))
            .iter()
            .filter(|name| name.starts_with("macro#pick#"))
            .count();
        assert_eq!(count, 2);
    }

    #[test]
    fn only_what_a_closure_reaches_is_kept_from_the_checked_package() {
        let built = build(
            concat!(
                "def used = value: Expr => Expr :: value\n",
                "def unused = value: Expr => Expr :: value\n",
                "macro m = value: Expr => used value\n",
                "let a = m 1\n",
            ),
            CheckMode::Invoked,
        );
        let functions = function_names(entry_items(&built));
        assert!(functions.contains(&"used".to_owned()), "{functions:?}");
        assert!(!functions.contains(&"unused".to_owned()), "{functions:?}");
    }

    #[test]
    fn every_macro_is_checked_with_all() {
        let built = build(
            "def keep = value: Expr => Expr :: value\nmacro never_called = value: Expr => keep value\n",
            CheckMode::All,
        );
        let functions = function_names(entry_items(&built));
        assert!(
            functions.iter().any(|name| name.starts_with("macro#never_called#")),
            "{functions:?}"
        );
    }

    #[test]
    fn only_invoked_macros_are_checked_with_one() {
        let built = build(
            concat!(
                "use std.clone.Clone\n",
            "def keep = value: Expr => Expr :: Clone.clone value\n",
                "macro invoked_template = value: Expr => Expr :: parse_quote { $value }\n",
                "macro idle = value: Expr => keep value\n",
                "let a = invoked_template 1\n",
            ),
            CheckMode::Invoked,
        );
        let functions = function_names(entry_items(&built));
        assert!(
            functions.iter().any(|name| name.starts_with("macro#invoked_template#")),
            "{functions:?}"
        );
        assert!(
            !functions.iter().any(|name| name.starts_with("macro#idle#")),
            "{functions:?}"
        );
        assert!(!functions.contains(&"keep".to_owned()), "{functions:?}");
    }

    #[test]
    fn a_cycle_in_a_macro_nothing_calls_only_fails_with_all() {
        let source = concat!(
            "def helper = value: Expr => Expr :: again value\n",
            "macro again = value: Expr => helper value\n",
            "def main = () => 0\n",
        );
        assert!(check_with(source, CheckMode::Invoked).is_empty());
        assert!(
            check_with(source, CheckMode::All)
                .iter()
                .any(|diagnostic| diagnostic.contains("`again` itself depends on")),
        );
    }

    #[test]
    fn dependencies_keep_their_compile_time_items() {
        let built = build(HELPER_MACRO, CheckMode::Invoked);
        let syntax = built
            .program
            .standard_library_syntax()
            .expect("std.syntax is loaded");
        let items = &built.program.module(syntax).syntax.items;
        assert!(items.iter().any(|item| matches!(item,
            Item::ExternBlock(block) if block.abi == "\"staple-intrinsic\"")));
        assert!(items.iter().any(|item| matches!(item,
            Item::TraitImplementation(implementation) if implementation.trait_name.name == "Parse")));
    }

    #[test]
    fn quotations_become_calls_with_a_template_table() {
        let built = build(
            concat!(
                "use std.clone.Clone\n",
            "def keep = value: Expr => Expr :: Clone.clone value\n",
                "def raw = value: Expr => Syntax :: quote { ($value) }\n",
                "macro twice = value: Expr => { let unused = raw value; keep (parse_quote { $value + $value }) }\n",
                "macro parsed: Expr -> Expr = value => keep (parse_quote { $value })\n",
                "let a = twice 1\nlet b = parsed 1\n",
            ),
            CheckMode::Invoked,
        );
        let entry = built.program.entry();
        // The table also holds the standard library's quotations.
        let templates = built
            .templates
            .templates
            .iter()
            .filter(|template| template.module == entry)
            .map(|template| template.contents.text())
            .collect::<Vec<_>>();
        assert_eq!(templates.len(), 3, "{templates:?}");
        assert!(templates.contains(&" $value + $value".to_owned()), "{templates:?}");
        assert!(templates.contains(&" $value".to_owned()), "{templates:?}");
        assert!(templates.contains(&" ($value)".to_owned()), "{templates:?}");
        // Entry functions splice results, not quotations.
        let quoting = entry_items(&built)
            .iter()
            .filter(|item| !matches!(item, Item::Binding(binding) if binding.name.starts_with("entry#")))
            .collect::<Vec<_>>();
        let rendered = format!("{quoting:?}");
        assert!(!rendered.contains("Quote("), "no quotation is left");
        assert!(rendered.contains("\"__quote\""));
        assert!(rendered.contains("\"parse_quote\""));
        // One splice per distinct name in each quotation, so `$value` twice in
        // one quotation is one splice.
        assert_eq!(rendered.matches("\"splice\"").count(), 3);
    }

    /// Expands `source` with the closure check on every macro, and returns what it reports.
    fn check_all(source: &str) -> Vec<String> {
        check_with(source, CheckMode::All)
    }

    fn check_with(source: &str, mode: CheckMode) -> Vec<String> {
        let source = format!("use std.syntax.*\n{source}");
        check_source(&source, mode)
    }

    fn check_source(source: &str, mode: CheckMode) -> Vec<String> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, &root)
            .expect("source should load");
        let (_, _, diagnostics) =
            super::super::expand_program_checked(program, true, Some(mode), true)
                .expect("source should expand");
        diagnostics.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_standard_library_macros_check_as_ordinary_code() {
        // Every standard-library macro is checked along with the program's own.
        let diagnostics = check_all("def main = () => 0\n");
        assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
    }

    #[test]
    fn the_macro_fixtures_check_as_ordinary_code() {
        // The compiler tests set this variable per process, which would race
        // with other tests, so one test checks all the fixture macros instead.
        for (name, source) in [
            ("conversions_as.sta", include_str!("../../tests/fixtures/conversions_as.sta")),
            ("macros.sta", include_str!("../../examples/macros.sta")),
        ] {
            let diagnostics = check_source(source, CheckMode::All);
            assert!(
                diagnostics.is_empty(),
                "{name}:\n{}",
                diagnostics.join("\n")
            );
        }
    }

    #[test]
    fn macros_of_every_shape_check_as_ordinary_code() {
        let diagnostics = check_all(concat!(
            "use std.clone.Clone\n",
            "def keep = value: Expr => Expr :: Clone.clone value\n",
            // One slot, several slots, overloads, and no quotation splices.
            "macro one: Expr -> Item = value => parse_quote { let one = $value }\n",
            "macro two: (Expr; Expr) -> Item = (a; b) => match a { _ => parse_quote { let two = 2 } }\n",
            "macro pick = move value: Expr => keep value\n",
            "macro pick = (move a: Expr; b: Expr) => keep a\n",
            // Metadata, with modifiers it may carry.
            "macro inspect modifiers (@traced): (MacroCallMetadata; Expr) -> Item = ((modifiers, _); value) => match modifiers {\n",
            "    Sequence () => parse_quote { let plain = $value },\n",
            "    _ => parse_quote { let traced = $value },\n",
            "}\n",
            // A modifier macro, a refinement pattern, and splices of a
            // sequence and of a separated list.
            "macro @mark: move Item -> Sequence<Item> = move item => Sequence (first: item, rest: Sequence ())\n",
            "macro apply: (Expr; Sequence<Expr>) -> Expr = (callee; arguments) =>\n",
            "    parse_quote { $callee $arguments }\n",
            "macro words: (Expr; Sequence<Expr>) -> Expr = (first; rest) => match first {\n",
            "    else_word: Ident<\"else\"> => {\n",
            "        let elements = Clone.clone rest\n",
            "        let value = Parenthesized (\n",
            "            Separated (separator: Comma, elements: elements, trailing: False),\n",
            "        )\n",
            "        parse_quote { $value }\n",
            "    },\n",
            "    other => {\n",
            "        let elements = Sequence (first: Clone.clone other, rest: Clone.clone rest)\n",
            "        let value = Parenthesized (\n",
            "            Separated (separator: Comma, elements: elements, trailing: False),\n",
            "        )\n",
            "        parse_quote { $value }\n",
            "    },\n",
            "}\n",
            "def main = () => 0\n",
        ));
        assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
    }

    #[test]
    fn a_quote_result_needs_no_annotation() {
        let diagnostics = check_all(concat!(
            "macro direct = value: Expr => quote { $value }\n",
            "macro in_a_block = move value: Expr => { let same = value; quote { let a = $same } }\n",
            "macro in_a_match = value: Expr => match value { _ => quote { $value } }\n",
            "def main = () => 0\n",
        ));
        assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
    }

    #[test]
    fn a_parse_quote_result_needs_a_type() {
        let diagnostics = check_all(concat!(
            "macro untyped = value: Expr => parse_quote { $value }\n",
            "def main = () => 0\n",
        ));
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.contains("macro `untyped`")),
            "{diagnostics:?}"
        );

        let diagnostics = check_all(concat!(
            "macro declared: Expr -> Expr = value => parse_quote { $value }\n",
            "macro ascribed = value: Expr => Expr :: parse_quote { $value }\n",
            "def main = () => 0\n",
        ));
        assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
    }

    #[test]
    fn splicing_a_value_that_is_not_syntax_is_a_type_error() {
        let diagnostics = check_all(concat!(
            "macro bad = value: Expr => { let count = 3; Expr :: parse_quote { $count } }\n",
            "def main = () => 0\n",
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("macro `bad`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn a_spaced_splice_is_a_splice() {
        let diagnostics = check_all(concat!(
            "macro bad = value: Expr => { let count = 3; Expr :: parse_quote { $ count } }\n",
            "def main = () => 0\n",
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("macro `bad`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn only_syntax_is_copied_freely() {
        let diagnostics = check_all(concat!(
            "use std.coroutine.Task\n",
            "def twice = (move handle: Task<I32>) => () :: {\n",
            "    let first = handle\n",
            "    let second = handle\n",
            "}\n",
            "macro uses = value: Expr => { let unused = twice; value }\n",
            "def main = () => 0\n",
        ));
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.contains("macro `uses`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn the_macro_runtime_intrinsics_are_a_closed_list() {
        assert!(super::super::is_macro_runtime_intrinsic("__quote"));
        assert!(super::super::is_macro_runtime_intrinsic("stream"));
        assert!(super::super::is_macro_runtime_intrinsic("__take_rest"));
        assert!(super::super::is_macro_runtime_intrinsic("__text_copy"));
        assert!(!super::super::is_macro_runtime_intrinsic("__splice_nod"));
    }

    #[test]
    fn imports_of_names_the_check_program_lacks_are_dropped() {
        let mut kind = UseKind::Selected(vec![SelectedImport::new("Kept"), SelectedImport::new("Gone")]);
        let defined = ["Kept".to_owned()].into_iter().collect();
        assert!(keep_import(&mut kind, &defined));
        assert_eq!(kind, UseKind::Selected(vec![SelectedImport::new("Kept")]));
        let mut kind = UseKind::Selected(vec![SelectedImport::new("Gone")]);
        assert!(!keep_import(&mut kind, &defined));
    }

    #[test]
    fn the_standard_library_checks_when_it_is_the_checked_package() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let stdlib = root.join("stdlib");
        let path = stdlib.join("std/alloc.sta");
        let source = std::fs::read_to_string(&path).expect("the module should be readable");
        let program = ProgramLoader::new()
            .with_standard_library_root(&stdlib)
            .load_source_at(&path, &source)
            .expect("the module should load");
        let (_, _, diagnostics) =
            super::super::expand_program_checked(program, true, Some(CheckMode::All), true)
                .expect("the standard library should expand");
        let rendered = diagnostics.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(rendered.is_empty(), "{}", rendered.join("\n"));
    }

    #[test]
    fn invoked_macros_of_a_standard_library_file_check_with_one() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let stdlib = root.join("stdlib");
        let path = stdlib.join("std/alloc.sta");
        let source = std::fs::read_to_string(&path).expect("the module should be readable");
        let program = ProgramLoader::new()
            .with_standard_library_root(&stdlib)
            .load_source_at(&path, &source)
            .expect("the module should load");
        let (_, _, diagnostics) =
            super::super::expand_program_checked(program, true, Some(CheckMode::Invoked), true)
                .expect("the standard library should expand");
        let rendered = diagnostics.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(rendered.is_empty(), "{}", rendered.join("\n"));
    }

    #[test]
    fn imports_of_submodules_are_kept() {
        let diagnostics = check_all(concat!(
            "mod inner {\n",
            "    use std.syntax.Expr\n",
            "    pub mod deeper {\n",
            "        use std.syntax.Expr\n",
            "        pub def f = move value: Expr => Expr :: value\n",
            "    }\n",
            "}\n",
            "use inner.(deeper)\n",
            "use inner.deeper as alias\n",
            "macro through_selected: move Expr -> Expr = move value => deeper.f value\n",
            "macro through_alias: move Expr -> Expr = move value => alias.f value\n",
            "let a: I32 = through_selected 1\n",
            "let b: I32 = through_alias 1\n",
        ));
        assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
    }

    #[test]
    fn package_rooted_imports_outside_the_closure_are_dropped_in_a_package() {
        let root = std::env::temp_dir().join(format!(
            "staple-check-package-imports-{}",
            std::process::id()
        ));
        let write = |path: &str, contents: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write("staple.kdl", "package \"app\"\n");
        write("src/main.sta", "use package.a.(m)\nlet x: I32 = m 1\n");
        write(
            "src/a.sta",
            concat!(
                "pub mod\n",
                "use std.syntax.Expr\n",
                "use package.b.(Kept)\n",
                "use package.c.(Idle)\n",
                "pub def idle_use = (value: Idle) => value\n",
                "def keep = move value: Expr => Expr :: { let kept: Kept = Kept 1; value }\n",
                "pub macro m: move Expr -> Expr = move value => keep value\n",
            ),
        );
        write("src/b.sta", "pub mod\npub type Kept wrap pub I32\n");
        write("src/c.sta", "pub mod\npub type Idle wrap pub I32\n");
        let graph = staple_project::load_package_graph(&root.join("staple.kdl")).unwrap();
        let program = ProgramLoader::new()
            .with_standard_library_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stdlib"),
            )
            .with_package_graph(graph)
            .load_package_graph()
            .expect("the package should load");
        let result = super::super::expand_program_checked(program, true, Some(CheckMode::All), true);
        std::fs::remove_dir_all(&root).ok();
        let (_, _, diagnostics) = result.expect("the package should expand");
        let rendered = diagnostics.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(rendered.is_empty(), "{}", rendered.join("\n"));
    }

    #[test]
    fn syntax_moves_are_reported_and_clone_makes_the_value_usable_again() {
        let diagnostics = check_all(concat!(
            "use std.clone.Clone\n",
            "macro use_after_move = (move value: Expr) => Expr :: { let moved = value; value }\n",
            "macro cloned = (move value: Expr) => Expr :: { let copied = Clone.clone value; value }\n",
            "def main = () => 0\n",
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("use of moved value")),
            "{diagnostics:?}"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("macro `cloned`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn ident_is_copy_while_sequences_of_ident_and_expr_are_not() {
        let diagnostics = check_all(concat!(
            "macro copy_ident = (move name: Ident<String>) => Ident<String> :: { let other = name; name }\n",
            "macro move_idents = (move values: Sequence<Ident<String>>) => Sequence<Ident<String>> :: { let moved = values; values }\n",
            "macro move_exprs = (move values: Sequence<Expr>) => Sequence<Expr> :: { let moved = values; values }\n",
            "def main = () => 0\n",
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("macro `copy_ident`")),
            "{diagnostics:?}"
        );
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.contains("use of moved value"))
                .count(),
            2,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn sequence_clone_impls_copy_ident_and_expr_elements() {
        let diagnostics = check_all(concat!(
            "use std.clone.Clone\n",
            "macro clone_idents = values: Sequence<Ident<String>> => Sequence<Ident<String>> :: Clone.clone values\n",
            "macro clone_exprs = values: Sequence<Expr> => Sequence<Expr> :: Clone.clone values\n",
            "def main = () => 0\n",
        ));
        assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
    }

    #[test]
    fn separated_is_not_copy_and_has_clone() {
        let diagnostics = check_all(concat!(
            "use std.clone.Clone\n",
            "def move_separated = (move values: Separated<Ident<String>, Comma>) => Separated<Ident<String>, Comma> :: { let moved = values; values }\n",
            "def clone_separated = values: Separated<Ident<String>, Comma> => Separated<Ident<String>, Comma> :: Clone.clone values\n",
            "macro uses_move = move value: Expr => { let unused = move_separated; value }\n",
            "macro uses_clone = move value: Expr => { let unused = clone_separated; value }\n",
            "def main = () => 0\n",
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("use of moved value")),
            "{diagnostics:?}"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("macro `uses_clone`")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn an_at_pattern_requires_a_copy_syntax_value() {
        let diagnostics = check_all(concat!(
            "macro bad = whole@value: Expr => whole\n",
            "def main = () => 0\n",
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("an `@` pattern requires a Copy value")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn a_sequence_of_non_syntax_values_cannot_be_spliced() {
        let diagnostics = check_all(concat!(
            "macro bad: Expr -> Syntax = _: Expr => {\n",
            "    let values: Sequence<I32> = Sequence (first: 1, rest: Sequence ())\n",
            "    quote { $values }\n",
            "}\n",
        ));
        assert!(
            !diagnostics.is_empty(),
            "non-syntax elements must have no Splice impl"
        );
        assert!(
            diagnostics.iter().any(|message| {
                message.contains("no trait implementation or matching bound is available")
            }),
            "{}",
            diagnostics.join("\n")
        );
    }

    #[test]
    fn borrowed_splices_and_repeated_splices_need_no_clone() {
        let diagnostics = check_all(concat!(
            "macro borrowed = value: Expr => Expr :: parse_quote { $value }\n",
            "macro repeated = value: Expr => Expr :: parse_quote { $value + $value }\n",
            "macro syntax_repeated = value: Syntax => Syntax :: quote { $value $value }\n",
            "def main = () => 0\n",
        ));
        assert!(diagnostics.is_empty(), "{}", diagnostics.join("\n"));
    }

    #[test]
    fn package_rooted_imports_resolve_by_their_dotted_module_name() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let stdlib = root.join("stdlib");
        let path = stdlib.join("std/ptr.sta");
        let source = std::fs::read_to_string(&path).expect("the module should be readable");
        let program = ProgramLoader::new()
            .with_standard_library_root(&stdlib)
            .load_source_at(&path, &source)
            .expect("the module should load");
        let module_names = program
            .modules()
            .iter()
            .filter_map(|module| {
                program
                    .module_dotted_name(module.id)
                    .map(|name| (name, module.id))
            })
            .collect::<Vec<_>>();
        let (source_module, declaration) = program
            .modules()
            .iter()
            .flat_map(|module| {
                module.syntax.items.iter().filter_map(move |item| match item {
                    Item::UseDeclaration(declaration)
                        if declaration.path.first().is_some_and(|part| part == "package")
                            && declaration.path.iter().any(|part| part == "hash") =>
                    {
                        Some((module.id, declaration))
                    }
                    _ => None,
                })
            })
            .next()
            .expect("ptr.sta imports package.hash");
        let target = check_import_target(&program, source_module, declaration, &module_names)
            .expect("the package-rooted import should resolve");
        assert_eq!(
            program.module_dotted_name(target).as_deref(),
            Some("std.hash"),
            "{module_names:?}"
        );
    }
}

/// A macro's first parameter is the call's metadata, which macro code writes
/// as a bare `(modifiers, visibility)`. As a function it is a `MacroCallMetadata`
/// like any other wrapper, so the pattern names it.
fn name_metadata_pattern(value: &mut Expression, syntax_module: ModuleId, next_syntax_id: &mut usize) {
    let Expression::Function(function) = value else {
        return;
    };
    let first = match &mut function.pattern {
        Pattern::Product(parameters) if parameters.slotted => parameters.elements.first_mut(),
        pattern => Some(pattern),
    };
    let Some(first) = first else {
        return;
    };
    if !matches!(first, Pattern::Product(_)) {
        return;
    }
    let argument = first.clone();
    let mut syntax = argument.syntax().clone();
    syntax.id = SyntaxId(*next_syntax_id);
    *next_syntax_id += 1;
    syntax.definition_module = Some(syntax_module.0);
    *first = Pattern::Nominal(staple_syntax::NominalPattern {
        syntax,
        namespace: None,
        name: "MacroCallMetadata".to_owned(),
        moved: false,
        argument: Box::new(argument),
        annotation: None,
    });
}

/// How many slots a macro signature's parameter has, when it is slotted.
fn slot_count(annotation: &staple_syntax::Type) -> Option<usize> {
    let staple_syntax::Type::Function(function) = annotation else {
        return None;
    };
    match function.parameter.as_ref() {
        staple_syntax::Type::Product(product) if product.slotted => Some(product.elements.len()),
        _ => None,
    }
}

/// A macro's parameters are written `(a; b) => body` but parsed one function
/// per slot. As a function they are one slotted parameter, as the signature
/// says, so the first `slots` functions become one.
fn merge_slots(value: &mut Expression, slots: usize, next_syntax_id: &mut usize) {
    let Expression::Function(outer) = value else {
        return;
    };
    let mut patterns = vec![outer.pattern.clone()];
    let mut body = outer.body.as_ref().clone();
    while patterns.len() < slots {
        let Expression::Function(inner) = body else {
            return;
        };
        patterns.push(inner.pattern.clone());
        body = inner.body.as_ref().clone();
    }
    let mut syntax = patterns[0].syntax().clone();
    syntax.id = SyntaxId(*next_syntax_id);
    *next_syntax_id += 1;
    outer.pattern = Pattern::Product(staple_syntax::ProductPattern {
        slotted: true,
        syntax,
        elements: patterns,
        mutable: false,
        moved: false,
        annotation: None,
    });
    *outer.body = body;
}
