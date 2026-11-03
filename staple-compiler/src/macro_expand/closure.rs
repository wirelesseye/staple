//! The item index and the compile-time closure of a macro.
//!
//! A macro's closure is every item that has to exist, expanded, for its body to
//! run: the helpers, types, traits, externs and `impl` items its body reaches
//! through names, operators and template macros. Macros it calls are recorded
//! separately as dependencies: their output cannot be known before they run,
//! so the closure is rescanned once the items are expanded.
use super::scan::{RawRef, Scan};
use super::*;
use std::collections::BTreeSet;

/// The unit of expansion: the syntax id of a top-level item of some module.
pub(crate) type ItemId = SyntaxId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ItemKind {
    Binding,
    Extern,
    Type,
    Trait,
    Macro,
    Impl,
    /// Anything else that can expand, such as an item-position macro invocation.
    Other,
}

pub(super) struct IndexedItem {
    pub(super) module: ModuleId,
    /// The item as last seen: the raw item, replaced by what it expanded to.
    pub(super) group: Rc<Vec<Item>>,
    pub(super) kind: ItemKind,
    /// Whether this is a modifier macro, which has its own namespace.
    pub(super) modifier: bool,
    pub(super) expanded: bool,
}

#[derive(Clone, Debug, Default)]
struct Resolved {
    items: Vec<ItemId>,
    macros: Vec<ItemId>,
    modifiers: Vec<ItemId>,
    quoted: Vec<ItemId>,
    quoted_macros: Vec<ItemId>,
}

struct ImplEntry {
    /// The unit that contains the implementation.
    origin: ItemId,
    traits: Vec<ItemId>,
    /// The type items its self type names; empty for a blanket implementation.
    selves: Vec<ItemId>,
    blanket: bool,
}

type Names = HashMap<String, Vec<ItemId>>;

/// What a macro needs, as read by later phases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MacroClosure {
    /// The items the macro's body reaches, including the macro's own item.
    pub(crate) items: BTreeSet<ItemId>,
    /// The macros and modifiers those items call, which must expand first.
    pub(crate) macros_used: BTreeSet<ItemId>,
}

/// The expander's name tables that resolution reads.
pub(super) struct Lookup<'a> {
    pub(super) scopes: &'a [ModuleScope],
    pub(super) parents: &'a [Option<ModuleId>],
    pub(super) companions: &'a [bool],
}

pub(super) struct ClosureIndex {
    pub(super) items: HashMap<ItemId, IndexedItem>,
    own: Vec<Names>,
    own_modifiers: Vec<Names>,
    exports: Vec<Names>,
    export_modifiers: Vec<Names>,
    visible: Vec<Names>,
    visible_modifiers: Vec<Names>,
    core: Option<ModuleId>,
    prelude: Option<ModuleId>,
    uses_prelude: Vec<bool>,
    resolved: HashMap<ItemId, Rc<Resolved>>,
    impls: Option<Rc<Vec<ImplEntry>>>,
    std_modules: StdModules,
}

fn item_kind(item: &Item) -> (ItemKind, bool, Vec<String>) {
    match item {
        Item::Modified(modified) => item_kind(&modified.item),
        Item::Binding(binding) => (ItemKind::Binding, false, vec![binding.name.clone()]),
        Item::ExternBlock(block) => (
            ItemKind::Extern,
            false,
            block
                .bindings
                .iter()
                .map(|binding| binding.name.clone())
                .collect(),
        ),
        Item::TypeDeclaration(declaration) => {
            (ItemKind::Type, false, vec![declaration.name.clone()])
        }
        Item::TraitDeclaration(declaration) => {
            (ItemKind::Trait, false, vec![declaration.name.clone()])
        }
        Item::MacroDeclaration(declaration) => (
            ItemKind::Macro,
            declaration.modifier,
            vec![declaration.name.clone()],
        ),
        Item::TraitImplementation(_) => (ItemKind::Impl, false, Vec::new()),
        _ => (ItemKind::Other, false, Vec::new()),
    }
}

/// Whether a module-level item is a unit of expansion. Imports and submodules
/// are structure rather than items.
pub(super) fn is_unit(item: &Item) -> bool {
    !matches!(item, Item::UseDeclaration(_) | Item::Submodule(_))
}

fn merge(into: &mut Names, from: &Names) -> bool {
    let mut changed = false;
    for (name, items) in from {
        let entry = into.entry(name.clone()).or_default();
        for item in items {
            if !entry.contains(item) {
                entry.push(*item);
                changed = true;
            }
        }
    }
    changed
}

fn add_name(table: &mut Names, name: &str, id: ItemId) -> bool {
    let entry = table.entry(name.to_owned()).or_default();
    if entry.contains(&id) {
        false
    } else {
        entry.push(id);
        true
    }
}

impl ClosureIndex {
    pub(super) fn build(
        program: &Program,
        use_kinds: &HashMap<SyntaxId, UseKind>,
        imported_modules: &HashMap<SyntaxId, ModuleId>,
        std_modules: StdModules,
    ) -> Self {
        let count = program.modules().len();
        let mut index = Self {
            items: HashMap::new(),
            own: vec![Names::new(); count],
            own_modifiers: vec![Names::new(); count],
            exports: Vec::new(),
            export_modifiers: Vec::new(),
            visible: Vec::new(),
            visible_modifiers: Vec::new(),
            core: program.standard_library_core(),
            prelude: program.standard_library_prelude(),
            uses_prelude: program
                .modules()
                .iter()
                .map(|module| program.module_uses_prelude(module.id))
                .collect(),
            resolved: HashMap::new(),
            impls: None,
            std_modules,
        };
        for module in program.modules() {
            for item in &module.syntax.items {
                if is_unit(item) {
                    let (kind, modifier, names) = item_kind(item);
                    let id = item.syntax().id;
                    for name in &names {
                        let table = if modifier {
                            &mut index.own_modifiers
                        } else {
                            &mut index.own
                        };
                        add_name(&mut table[module.id.0], name, id);
                    }
                    index.items.insert(
                        id,
                        IndexedItem {
                            module: module.id,
                            group: Rc::new(vec![item.clone()]),
                            kind,
                            modifier,
                            expanded: false,
                        },
                    );
                }
            }
        }
        index.rebuild(program, use_kinds, imported_modules);
        index
    }

    /// Recomputes what each module exports and sees from its own definitions
    /// and its `use` declarations. Visibility is ignored, which can only make a
    /// closure larger.
    pub(super) fn rebuild(
        &mut self,
        program: &Program,
        use_kinds: &HashMap<SyntaxId, UseKind>,
        imported_modules: &HashMap<SyntaxId, ModuleId>,
    ) {
        self.exports = self.own.clone();
        self.export_modifiers = self.own_modifiers.clone();
        loop {
            let mut changed = false;
            for module in program.modules() {
                for item in &module.syntax.items {
                    let Item::UseDeclaration(declaration) = item else {
                        continue;
                    };
                    let (Some(imported), Some(kind)) = (
                        imported_modules.get(&declaration.syntax.id).copied(),
                        use_kinds.get(&declaration.syntax.id),
                    ) else {
                        continue;
                    };
                    let mut names = Names::new();
                    let mut modifiers = Names::new();
                    match kind {
                        UseKind::Dotted | UseKind::Namespace => {}
                        UseKind::Glob => {
                            names = self.exports[imported.0].clone();
                            modifiers = self.export_modifiers[imported.0].clone();
                        }
                        UseKind::Selected(selected) => {
                            for import in selected {
                                self.select(imported, &import.name, import.alias(), &mut names, &mut modifiers);
                            }
                        }
                        UseKind::Renamed { item, alias } => {
                            self.select(imported, item, alias, &mut names, &mut modifiers)
                        }
                    }
                    changed |= merge(&mut self.exports[module.id.0], &names);
                    changed |= merge(&mut self.export_modifiers[module.id.0], &modifiers);
                }
            }
            if !changed {
                break;
            }
        }
        self.visible = self.exports.clone();
        self.visible_modifiers = self.export_modifiers.clone();
        for module in 0..program.modules().len() {
            for implicit in [self.core, self.prelude.filter(|_| self.uses_prelude[module])]
                .into_iter()
                .flatten()
            {
                if implicit.0 == module {
                    continue;
                }
                let (names, modifiers) = (
                    self.exports[implicit.0].clone(),
                    self.export_modifiers[implicit.0].clone(),
                );
                merge(&mut self.visible[module], &names);
                merge(&mut self.visible_modifiers[module], &modifiers);
            }
        }
        self.resolved.clear();
        self.impls = None;
    }

    fn select(
        &self,
        imported: ModuleId,
        name: &str,
        alias: &str,
        names: &mut Names,
        modifiers: &mut Names,
    ) {
        if let Some(items) = self.exports[imported.0].get(name) {
            names.insert(alias.to_owned(), items.clone());
        }
        if let Some(items) = self.export_modifiers[imported.0].get(name) {
            modifiers.insert(alias.to_owned(), items.clone());
        }
    }

    /// Records what `origin` expanded to. Names the expansion introduces are
    /// attributed to `origin`, so reaching one of them reaches the unit that
    /// makes it. Returns whether a name is new, in which case the caller
    /// rebuilds the name tables with [`Self::rebuild`].
    pub(super) fn record_expansion(&mut self, origin: ItemId, expanded: &[Item]) -> bool {
        let Some(indexed) = self.items.get_mut(&origin) else {
            return false;
        };
        let module = indexed.module;
        indexed.group = Rc::new(expanded.to_vec());
        indexed.expanded = true;
        let mut introduced = false;
        for item in expanded {
            let (_, modifier, names) = item_kind(item);
            for name in names {
                let table = if modifier {
                    &mut self.own_modifiers
                } else {
                    &mut self.own
                };
                introduced |= add_name(&mut table[module.0], &name, origin);
            }
        }
        self.resolved.remove(&origin);
        self.impls = None;
        introduced
    }

    // ----- resolution -----

    fn namespace(lookup: &Lookup, module: ModuleId, name: &str) -> Option<ModuleId> {
        let mut cursor = module;
        loop {
            if let Some(target) = lookup.scopes[cursor.0].namespaces.get(name) {
                return Some(*target);
            }
            if !lookup.companions[cursor.0] {
                return None;
            }
            cursor = lookup.parents[cursor.0]?;
        }
    }

    fn visible_in<'a>(
        &'a self,
        lookup: &Lookup,
        module: ModuleId,
        name: &str,
        modifier: bool,
    ) -> Option<&'a Vec<ItemId>> {
        let table = if modifier {
            &self.visible_modifiers
        } else {
            &self.visible
        };
        let mut cursor = module;
        loop {
            if let Some(items) = table[cursor.0].get(name) {
                return Some(items);
            }
            if !lookup.companions[cursor.0] {
                return None;
            }
            cursor = lookup.parents[cursor.0]?;
        }
    }

    /// The items a written name can mean. Every item sharing the name counts,
    /// whatever its kind, which can only make a closure larger.
    fn resolve_ref(
        &self,
        lookup: &Lookup,
        module: ModuleId,
        reference: &RawRef,
        modifier: bool,
    ) -> Vec<ItemId> {
        let context = reference.definition.unwrap_or(module);
        let Some(first) = reference.path.first() else {
            return Vec::new();
        };
        let mut found = Vec::new();
        if let Some(items) = self.visible_in(lookup, context, first, modifier) {
            found.extend(items.iter().copied());
        }
        let mut namespace = Self::namespace(lookup, context, first);
        for segment in &reference.path[1..] {
            let Some(target) = namespace else { break };
            let (exports, _) = if modifier {
                (&self.export_modifiers, ())
            } else {
                (&self.exports, ())
            };
            if let Some(items) = exports[target.0].get(segment) {
                found.extend(items.iter().copied());
            }
            namespace = lookup.scopes[target.0].namespaces.get(segment).copied();
        }
        found
    }

    fn resolved(&mut self, lookup: &Lookup, id: ItemId) -> Rc<Resolved> {
        if let Some(resolved) = self.resolved.get(&id) {
            return resolved.clone();
        }
        let Some(indexed) = self.items.get(&id) else {
            return Rc::new(Resolved::default());
        };
        let (module, group) = (indexed.module, indexed.group.clone());
        let mut scan = Scan::default();
        for item in group.iter() {
            let mut item = item.clone();
            let mut next_syntax_id = usize::MAX / 2;
            desugar_item(&mut item, &mut next_syntax_id, self.std_modules);
            let part = Scan::item(&item);
            scan.refs.extend(part.refs);
            scan.quoted.extend(part.quoted);
            scan.modifiers.extend(part.modifiers);
        }
        let mut resolved = Resolved::default();
        let sort = |references: &[RawRef], items: &mut Vec<ItemId>, macros: &mut Vec<ItemId>, this: &Self| {
            for reference in references {
                for target in this.resolve_ref(lookup, module, reference, false) {
                    if target == id {
                        continue;
                    }
                    let list = match this.items.get(&target).map(|indexed| indexed.kind) {
                        Some(ItemKind::Macro) => &mut *macros,
                        Some(_) => &mut *items,
                        None => continue,
                    };
                    if !list.contains(&target) {
                        list.push(target);
                    }
                }
            }
        };
        sort(&scan.refs, &mut resolved.items, &mut resolved.macros, self);
        sort(
            &scan.quoted,
            &mut resolved.quoted,
            &mut resolved.quoted_macros,
            self,
        );
        for reference in &scan.modifiers {
            for target in self.resolve_ref(lookup, module, reference, true) {
                if target != id && !resolved.modifiers.contains(&target) {
                    resolved.modifiers.push(target);
                }
            }
        }
        let resolved = Rc::new(resolved);
        self.resolved.insert(id, resolved.clone());
        resolved
    }

    fn impl_entries(&mut self, lookup: &Lookup) -> Rc<Vec<ImplEntry>> {
        if let Some(entries) = &self.impls {
            return entries.clone();
        }
        let mut entries = Vec::new();
        let mut origins: Vec<ItemId> = self.items.keys().copied().collect();
        origins.sort();
        for origin in origins {
            let indexed = &self.items[&origin];
            let (module, group) = (indexed.module, indexed.group.clone());
            for item in group.iter() {
                let Item::TraitImplementation(implementation) = traits_plain(item) else {
                    continue;
                };
                let trait_ref = named_ref(&implementation.trait_name);
                let traits = self
                    .resolve_ref(lookup, module, &trait_ref, false)
                    .into_iter()
                    .filter(|target| self.items.get(target).is_some_and(|item| item.kind == ItemKind::Trait))
                    .collect::<Vec<_>>();
                let parameters = implementation
                    .type_parameters
                    .iter()
                    .flat_map(|parameter| parameter.names())
                    .collect::<HashSet<_>>();
                let mut heads = Vec::new();
                if let Some(self_type) = implementation.arguments.first() {
                    collect_type_heads(self_type, &mut heads);
                }
                let blanket = matches!(
                    implementation.arguments.first(),
                    Some(Type::Named(named)) if named.namespace.is_none() && parameters.contains(named.name.as_str())
                );
                let selves = heads
                    .into_iter()
                    .filter(|head| !(head.namespace.is_none() && parameters.contains(head.name.as_str())))
                    .flat_map(|head| self.resolve_ref(lookup, module, &named_ref(&head), false))
                    .filter(|target| self.items.get(target).is_some_and(|item| item.kind == ItemKind::Type))
                    .collect::<Vec<_>>();
                entries.push(ImplEntry {
                    origin,
                    traits,
                    selves,
                    blanket,
                });
            }
        }
        let entries = Rc::new(entries);
        self.impls = Some(entries.clone());
        entries
    }

    /// The closure of the macro declared by `start`. `is_template` says which
    /// macros are templates: a template's quoted names count as references of
    /// the item that calls it.
    pub(super) fn closure(
        &mut self,
        lookup: &Lookup,
        start: ItemId,
        is_template: &dyn Fn(ItemId) -> bool,
    ) -> MacroClosure {
        let mut closure = MacroClosure::default();
        closure.items.insert(start);
        let mut work = vec![start];
        let entries = self.impl_entries(lookup);
        loop {
            while let Some(next) = work.pop() {
                let resolved = self.resolved(lookup, next);
                for target in &resolved.items {
                    if closure.items.insert(*target) {
                        work.push(*target);
                    }
                }
                let called = resolved.macros.iter().chain(&resolved.modifiers);
                for target in called {
                    closure.macros_used.insert(*target);
                    if is_template(*target) {
                        let template = self.resolved(lookup, *target);
                        for item in &template.quoted {
                            if closure.items.insert(*item) {
                                work.push(*item);
                            }
                        }
                    }
                }
            }
            let mut added = false;
            for entry in entries.iter() {
                if closure.items.contains(&entry.origin) {
                    continue;
                }
                let has_trait = entry.traits.iter().any(|item| closure.items.contains(item));
                let has_self = entry.blanket
                    || entry.selves.is_empty()
                    || entry.selves.iter().all(|item| closure.items.contains(item));
                if has_trait && has_self {
                    closure.items.insert(entry.origin);
                    work.push(entry.origin);
                    added = true;
                }
            }
            if !added && work.is_empty() {
                break;
            }
        }
        closure
    }
}

fn traits_plain(item: &Item) -> &Item {
    match item {
        Item::Modified(modified) => traits_plain(&modified.item),
        _ => item,
    }
}

fn named_ref(named: &staple_syntax::NamedType) -> RawRef {
    let mut path = Vec::new();
    if let Some(namespace) = &named.namespace {
        path.extend(namespace.split('.').map(str::to_owned));
    }
    path.push(named.name.clone());
    RawRef {
        path,
        definition: named.syntax.definition_module().map(ModuleId),
    }
}

fn collect_type_heads(ty: &Type, heads: &mut Vec<staple_syntax::NamedType>) {
    match ty {
        Type::Named(named) => heads.push(named.clone()),
        Type::Product(product) => {
            for element in &product.elements {
                collect_type_heads(&element.ty, heads);
            }
        }
        Type::Sum(sum) => {
            for alternative in &sum.alternatives {
                collect_type_heads(alternative, heads);
            }
        }
        Type::Function(function) => {
            collect_type_heads(&function.parameter, heads);
            collect_type_heads(&function.result, heads);
        }
        Type::Application(application) => {
            collect_type_heads(&application.callee, heads);
            collect_type_heads(&application.argument, heads);
        }
        Type::Array(array) => {
            collect_type_heads(&array.element, heads);
            collect_type_heads(&array.count, heads);
        }
        Type::EffectApplication(application) => collect_type_heads(&application.callee, heads),
        Type::Forall(forall) => collect_type_heads(&forall.body, heads),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProgramLoader;
    use std::path::PathBuf;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "staple-closure-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, relative: &str, source: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The item names in the closure of `macro_name`, with implementations
    /// written as `impl <Trait>`, and the names of the macros it calls.
    fn closure_of(fixture: &Fixture, macro_name: &str) -> (Vec<String>, Vec<String>) {
        let program = ProgramLoader::new()
            .with_standard_library_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stdlib"),
            )
            .load_path(&fixture.root.join("main.sta"))
            .expect("fixture should load");
        let mut expander = MacroExpander::new(&program);
        let id = expander
            .definitions
            .values()
            .find(|definition| definition.key.name == macro_name && !definition.key.modifier)
            .map(|definition| definition.declaration.syntax.id)
            .expect("the macro is declared");
        let closure = expander.macro_closure(id);
        let describe = |id: &ItemId| -> Vec<String> {
            expander.closure.items[id]
                .group
                .iter()
                .flat_map(|item| match traits_plain(item) {
                    Item::TraitImplementation(implementation) => {
                        vec![format!("impl {}", implementation.trait_name.name)]
                    }
                    item => item_kind(item).2,
                })
                .collect()
        };
        (
            closure.items.iter().flat_map(describe).collect(),
            closure.macros_used.iter().flat_map(describe).collect(),
        )
    }

    fn contains(names: &[String], name: &str) -> bool {
        names.iter().any(|candidate| candidate == name)
    }

    const HEADER: &str = "use std.syntax.(quote, parse_quote, Expr)\n";

    #[test]
    fn a_closure_reaches_helpers_types_and_traits() {
        let fixture = Fixture::new("kinds");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}trait Shape A {{ area: A -> I32 }}\ntype Square wrap I32\n\
                 def unused = (x: I32) => x\n\
                 def helper = (s: Square) => I32 :: 1\n\
                 macro m = value: Expr => {{ let n = helper; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(contains(&items, "helper"), "{items:?}");
        assert!(contains(&items, "Square"), "{items:?}");
        assert!(!contains(&items, "unused"), "{items:?}");
        assert!(!contains(&items, "Shape"), "{items:?}");
    }

    #[test]
    fn locals_shadow_items() {
        let fixture = Fixture::new("locals");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}def shadowed = (x: I32) => x\n\
                 macro m = value: Expr => {{ let shadowed = 1; let _ = shadowed; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(!contains(&items, "shadowed"), "{items:?}");
    }

    #[test]
    fn quotations_are_not_dependencies_but_template_calls_are() {
        let fixture = Fixture::new("quotes");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}def quoted_helper = (x: I32) => x\n\
                 def templated_helper = (x: I32) => x\n\
                 macro tmpl = value: Expr => parse_quote {{ templated_helper $value }}\n\
                 macro m = value: Expr => parse_quote {{ quoted_helper $value }}\n\
                 macro n = value: Expr => {{ let a = tmpl 1; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(!contains(&items, "quoted_helper"), "{items:?}");
        let (items, macros) = closure_of(&fixture, "n");
        assert!(contains(&macros, "tmpl"), "{macros:?}");
        assert!(contains(&items, "templated_helper"), "{items:?}");
    }

    #[test]
    fn a_closure_follows_imports_renames_and_reexports() {
        let fixture = Fixture::new("imports");
        fixture.write("inner.sta", "pub mod\npub def deep = (x: I32) => x\n");
        fixture.write("outer.sta", "pub mod\npub use inner.(deep as renamed)\n");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}use outer.(renamed)\n\
                 macro m = value: Expr => {{ let a = renamed; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(contains(&items, "deep"), "{items:?}");
    }

    #[test]
    fn a_closure_follows_namespaces_and_companions() {
        let fixture = Fixture::new("namespaces");
        fixture.write("tools.sta", "pub mod\npub def made = (x: I32) => x\npub def other = (x: I32) => x\n");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}use tools\n\
                 macro m = value: Expr => {{ let a = tools.made; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(contains(&items, "made"), "{items:?}");
        assert!(!contains(&items, "other"), "{items:?}");
    }

    #[test]
    fn operators_reach_their_traits_and_implementations() {
        let fixture = Fixture::new("operators");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}macro m = value: Expr => {{ let a = 1 + 2; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(contains(&items, "Add"), "{items:?}");
        assert!(contains(&items, "impl Add"), "{items:?}");
    }

    #[test]
    fn concrete_and_blanket_implementations_follow_their_trait() {
        let fixture = Fixture::new("impls");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}trait Tag A {{ tag: A -> I32 }}\n\
                 type Used wrap I32\ntype Unused wrap I32\n\
                 impl Tag Used {{ tag = (x: Used) => I32 :: 1 }}\n\
                 impl Tag Unused {{ tag = (x: Unused) => I32 :: 2 }}\n\
                 def helper = (x: Used) => Tag.tag x\n\
                 macro m = value: Expr => {{ let a = helper; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(contains(&items, "Tag"), "{items:?}");
        assert_eq!(items.iter().filter(|name| *name == "impl Tag").count(), 1, "{items:?}");
    }

    #[test]
    #[ignore = "prints closures of standard-library macros"]
    fn print_standard_library_closures() {
        let fixture = Fixture::new("print");
        fixture.write("main.sta", "let x: I32 = 1\n");
        for name in ["if", "when", "while", "for", "derive_eq", "typegroup"] {
            let program = ProgramLoader::new()
                .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stdlib"))
                .load_path(&fixture.root.join("main.sta"))
                .unwrap();
            let mut expander = MacroExpander::new(&program);
            let Some(id) = expander
                .definitions
                .values()
                .find(|definition| definition.key.name == name)
                .map(|definition| definition.declaration.syntax.id)
            else {
                continue;
            };
            let closure = expander.macro_closure(id);
            let describe = |id: &ItemId| -> String {
                expander.closure.items[id]
                    .group
                    .iter()
                    .flat_map(|item| match traits_plain(item) {
                        Item::TraitImplementation(i) => vec![format!("impl {}", i.trait_name.name)],
                        item => item_kind(item).2,
                    })
                    .collect::<Vec<_>>()
                    .join("+")
            };
            let items: Vec<_> = closure.items.iter().map(describe).collect();
            let macros: Vec<_> = closure.macros_used.iter().map(describe).collect();
            eprintln!("{name}: {} items, macros {macros:?}\n  {items:?}", items.len());
        }
    }
}
