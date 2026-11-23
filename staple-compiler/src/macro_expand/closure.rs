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
    /// Where the item sits in its module's written items.
    pub(super) position: usize,
    /// For one binding of an `extern` block, which one. Each binding is its
    /// own item, so reaching one does not reach the rest of its block.
    pub(super) part: Option<usize>,
    /// Whether expanding the item can define names it does not define as
    /// written: an item-position macro invocation, or an item with modifiers.
    pub(super) generates: bool,
    /// What the item expanded to, once it has. Until then the item is read
    /// from the written items.
    pub(super) group: Option<Rc<Vec<Item>>>,
    pub(super) kind: ItemKind,
    /// Whether this is a modifier macro, which has its own namespace.
    pub(super) modifier: bool,
    pub(super) expanded: bool,
    /// How the item is named in messages: what it defines, or its kind.
    pub(super) label: String,
    /// Where the item is written, for diagnostics.
    pub(super) span: Span,
}

#[derive(Clone, Debug, Default)]
struct Resolved {
    items: Vec<ItemId>,
    macros: Vec<ItemId>,
    modifiers: Vec<ItemId>,
    quoted: Vec<ItemId>,
    quoted_macros: Vec<ItemId>,
    /// Written names that resolve to nothing in the item's module, which an
    /// item-position macro there may still generate.
    unresolved: Vec<(ModuleId, String)>,
    /// Names used as methods, which resolve against companion functions.
    methods: Vec<String>,
}

struct ImplEntry {
    /// The unit that contains the implementation.
    origin: ItemId,
    traits: Vec<ItemId>,
    /// The type items its self type names; empty for a blanket implementation.
    selves: Vec<ItemId>,
    blanket: bool,
}

/// A companion function, with what decides whether a method call of its name
/// can reach it.
struct CompanionEntry {
    /// The type items its companion's target names.
    selves: Vec<ItemId>,
    /// For a blanket companion (`companion<T where Bound T> T`), the traits
    /// its parameter is bounded by.
    blanket_traits: Vec<ItemId>,
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
    uses: Vec<Vec<SyntaxId>>,
    /// The unit that holds each binding, trait or implementation, by the id of
    /// the declaration itself rather than of a modifier wrapped around it.
    inner: HashMap<SyntaxId, ItemId>,
    /// The companion functions, by name: any of them may be what a method
    /// call or pipe stage of that name runs.
    methods: HashMap<String, Vec<ItemId>>,
    /// The items as written, by module, until the module walk takes them.
    raw: Vec<Vec<Option<Item>>>,
    own: Vec<Names>,
    own_modifiers: Vec<Names>,
    exports: Vec<Names>,
    export_modifiers: Vec<Names>,
    core: Option<ModuleId>,
    prelude: Option<ModuleId>,
    uses_prelude: Vec<bool>,
    resolved: HashMap<ItemId, Rc<Resolved>>,
    impls: HashMap<ItemId, Rc<Vec<ImplEntry>>>,
    companions: HashMap<ItemId, Rc<Vec<CompanionEntry>>>,
    /// The units that currently contain an implementation.
    impl_origins: BTreeSet<ItemId>,
    std_modules: StdModules,
}

pub(super) fn item_kind(item: &Item) -> (ItemKind, bool, Vec<String>) {
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
            uses: vec![Vec::new(); count],
            inner: HashMap::new(),
            methods: HashMap::new(),
            raw: (0..count).map(|_| Vec::new()).collect(),
            own: vec![Names::new(); count],
            own_modifiers: vec![Names::new(); count],
            exports: Vec::new(),
            export_modifiers: Vec::new(),
            core: program.standard_library_core(),
            prelude: program.standard_library_prelude(),
            uses_prelude: program
                .modules()
                .iter()
                .map(|module| program.module_uses_prelude(module.id))
                .collect(),
            resolved: HashMap::new(),
            impls: HashMap::new(),
            companions: HashMap::new(),
            impl_origins: BTreeSet::new(),
            std_modules,
        };
        for module in program.modules() {
            for (position, item) in module.syntax.items.iter().enumerate() {
                if let Item::UseDeclaration(declaration) = item {
                    index.uses[module.id.0].push(declaration.syntax.id);
                }
                if let Item::ExternBlock(block) = item
                    && block.bindings.len() > 1
                {
                    for (part, binding) in block.bindings.iter().enumerate() {
                        let id = binding.syntax.id;
                        add_name(&mut index.own[module.id.0], &binding.name, id);
                        if binding.companion_target.is_some() {
                            index
                                .methods
                                .entry(binding.name.clone())
                                .or_default()
                                .push(id);
                        }
                        index.items.insert(
                            id,
                            IndexedItem {
                                module: module.id,
                                position,
                                part: Some(part),
                                generates: false,
                                group: None,
                                kind: ItemKind::Extern,
                                modifier: false,
                                expanded: true,
                                label: binding.name.clone(),
                                span: binding.syntax.span.clone(),
                            },
                        );
                    }
                    index.items.insert(
                        item.syntax().id,
                        IndexedItem {
                            module: module.id,
                            position,
                            part: None,
                            generates: false,
                            group: None,
                            kind: ItemKind::Other,
                            modifier: false,
                            expanded: false,
                            label: format!("extern block #{position}"),
                            span: item.syntax().span.clone(),
                        },
                    );
                    continue;
                }
                if is_unit(item) {
                    let (kind, modifier, names) = item_kind(item);
                    let id = item.syntax().id;
                    match traits_plain(item) {
                        Item::Binding(binding) => {
                            index.inner.insert(binding.syntax.id, id);
                        }
                        Item::TraitImplementation(implementation) => {
                            index.inner.insert(implementation.syntax.id, id);
                        }
                        Item::TraitDeclaration(declaration) => {
                            index.inner.insert(declaration.syntax.id, id);
                        }
                        _ => {}
                    }
                    for binding in companion_bindings(item) {
                        index
                            .methods
                            .entry(binding.name.clone())
                            .or_default()
                            .push(id);
                    }
                    if kind == ItemKind::Impl {
                        index.impl_origins.insert(id);
                    }
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
                            position,
                            part: None,
                            generates: kind == ItemKind::Other
                                || matches!(item, Item::Modified(modified)
                                    if !modified.modifiers.iter().all(|modifier| modifier.namespace.is_none() && modifier.name == "doc")),
                            group: None,
                            kind,
                            modifier,
                            expanded: false,
                            label: if names.is_empty() {
                                format!("{kind:?} #{position}").to_lowercase()
                            } else {
                                names.join("+")
                            },
                            span: item.syntax().span.clone(),
                        },
                    );
                }
            }
        }
        for namespace in program.modules().iter().filter(|module| module.namespace) {
            for block in program.companion_blocks(namespace.id) {
                if block == namespace.id {
                    continue;
                }
                let block_own = index.own[block.0].clone();
                for (name, ids) in block_own {
                    for id in ids {
                        add_name(&mut index.own[namespace.id.0], &name, id);
                    }
                }
                let block_modifiers = index.own_modifiers[block.0].clone();
                for (name, ids) in block_modifiers {
                    for id in ids {
                        add_name(&mut index.own_modifiers[namespace.id.0], &name, id);
                    }
                }
            }
        }
        index.rebuild(use_kinds, imported_modules);
        index
    }

    /// Recomputes what each module exports and sees from its own definitions
    /// and its `use` declarations. Visibility is ignored, which can only make a
    /// closure larger.
    pub(super) fn rebuild(
        &mut self,
        use_kinds: &HashMap<SyntaxId, UseKind>,
        imported_modules: &HashMap<SyntaxId, ModuleId>,
    ) {
        self.exports = self.own.clone();
        self.export_modifiers = self.own_modifiers.clone();
        let mut version = vec![0usize; self.own.len()];
        let mut seen: HashMap<SyntaxId, usize> = HashMap::new();
        loop {
            let mut changed = false;
            for module in 0..self.own.len() {
                for use_id in self.uses[module].clone() {
                    let (Some(imported), Some(kind)) = (
                        imported_modules.get(&use_id).copied(),
                        use_kinds.get(&use_id),
                    ) else {
                        continue;
                    };
                    if seen.get(&use_id) == Some(&version[imported.0]) {
                        continue;
                    }
                    seen.insert(use_id, version[imported.0]);
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
                                self.select(
                                    imported,
                                    &import.name,
                                    import.alias(),
                                    &mut names,
                                    &mut modifiers,
                                );
                            }
                        }
                        UseKind::Renamed { item, alias } => {
                            self.select(imported, item, alias, &mut names, &mut modifiers)
                        }
                    }
                    let grew = merge(&mut self.exports[module], &names)
                        | merge(&mut self.export_modifiers[module], &modifiers);
                    if grew {
                        version[module] += 1;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // A name that resolved still does, so only what mentions an unresolved
        // name needs scanning again.
        self.resolved
            .retain(|_, resolved| resolved.unresolved.is_empty());
        self.impls.clear();
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
    pub(super) fn record_expansion(
        &mut self,
        origin: ItemId,
        expanded: Rc<Vec<Item>>,
        complete: bool,
    ) -> bool {
        let Some(indexed) = self.items.get_mut(&origin) else {
            return false;
        };
        let module = indexed.module;
        indexed.group = Some(expanded.clone());
        indexed.expanded = complete;
        if expanded
            .iter()
            .any(|item| matches!(traits_plain(item), Item::TraitImplementation(_)))
        {
            self.impl_origins.insert(origin);
        } else {
            self.impl_origins.remove(&origin);
        }
        let mut introduced = false;
        for item in expanded.iter() {
            for binding in companion_bindings(item) {
                let entry = self.methods.entry(binding.name.clone()).or_default();
                if !entry.contains(&origin) {
                    entry.push(origin);
                }
            }
            let (_, modifier, names) = item_kind(item);
            for name in names {
                let table = if modifier {
                    &mut self.own_modifiers
                } else {
                    &mut self.own
                };
                // A name the module already defines is not made by this unit,
                // which may only be the whole of a block that was split up.
                if !table[module.0].contains_key(&name) {
                    introduced |= add_name(&mut table[module.0], &name, origin);
                }
            }
        }
        self.resolved.remove(&origin);
        self.impls.remove(&origin);
        self.companions.remove(&origin);
        introduced
    }

    /// Moves a module's written items into the index. `copy` leaves the
    /// module's own list in place, for a module the walk reads from there.
    pub(super) fn load_raw(&mut self, module: ModuleId, items: Vec<Item>) {
        self.raw[module.0] = items.into_iter().map(Some).collect();
    }

    /// Takes the written item at `position`, if it has not been taken.
    pub(super) fn take_raw(&mut self, module: ModuleId, position: usize) -> Option<Item> {
        self.raw[module.0].get_mut(position).and_then(Option::take)
    }

    pub(super) fn raw_len(&self, module: ModuleId) -> usize {
        self.raw[module.0].len()
    }

    /// Runs `read` over what the unit `id` currently is, without copying the
    /// item when it is still as written.
    fn with_group<R>(&self, id: ItemId, read: impl FnOnce(&[Item]) -> R) -> R {
        let indexed = &self.items[&id];
        if let Some(group) = &indexed.group {
            return read(group);
        }
        match (
            self.raw[indexed.module.0]
                .get(indexed.position)
                .and_then(Option::as_ref),
            indexed.part,
        ) {
            (Some(item), None) => read(std::slice::from_ref(item)),
            _ => read(&self.group_of(id)),
        }
    }

    /// What the unit `id` currently is: its expansion, or the item as written.
    pub(super) fn group_of(&self, id: ItemId) -> Rc<Vec<Item>> {
        let indexed = &self.items[&id];
        match &indexed.group {
            Some(group) => group.clone(),
            None => {
                let written = self.raw[indexed.module.0]
                    .get(indexed.position)
                    .and_then(|item| item.clone());
                match (written, indexed.part) {
                    (Some(Item::ExternBlock(mut block)), Some(part)) => {
                        let binding = block.bindings.swap_remove(part);
                        block.bindings = vec![binding];
                        Rc::new(vec![Item::ExternBlock(block)])
                    }
                    (written, _) => Rc::new(written.into_iter().collect()),
                }
            }
        }
    }

    /// The unit that holds the declaration with syntax id `inner`.
    pub(super) fn unit_of(&self, inner: SyntaxId) -> Option<ItemId> {
        let unit = self.inner.get(&inner).copied().unwrap_or(inner);
        self.items.contains_key(&unit).then_some(unit)
    }

    /// Keeps `item`, which the walk has taken, readable as the unit `id` until
    /// its expansion is recorded.
    pub(super) fn keep_written(&mut self, id: ItemId, item: &Item) {
        if let Some(indexed) = self.items.get_mut(&id) {
            indexed.group = Some(Rc::new(vec![item.clone()]));
        }
        self.impls.remove(&id);
    }

    /// Drops everything the module walk no longer needs.
    pub(super) fn release(&mut self) {
        self.items.clear();
        self.raw.clear();
        self.resolved.clear();
        self.impls.clear();
        self.companions.clear();
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
            &self.export_modifiers
        } else {
            &self.exports
        };
        let mut cursor = module;
        loop {
            if let Some(items) = table[cursor.0].get(name) {
                return Some(items);
            }
            if !lookup.companions[cursor.0] {
                break;
            }
            let Some(parent) = lookup.parents[cursor.0] else {
                break;
            };
            cursor = parent;
        }
        // The implicit imports come last, so they never shadow a written name.
        [
            self.core,
            self.prelude.filter(|_| self.uses_prelude[module.0]),
        ]
        .into_iter()
        .flatten()
        .filter(|implicit| *implicit != module)
        .find_map(|implicit| table[implicit.0].get(name))
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
            {
                let module = target;
                if let Some(items) = exports[module.0].get(segment) {
                    found.extend(items.iter().copied());
                }
            }
            namespace = lookup.scopes[target.0].namespaces.get(segment).copied();
        }
        found
    }

    fn resolved(&mut self, lookup: &Lookup, id: ItemId) -> Rc<Resolved> {
        if let Some(resolved) = self.resolved.get(&id) {
            return resolved.clone();
        }
        self.resolved_uncached(lookup, id)
    }

    fn resolved_uncached(&mut self, lookup: &Lookup, id: ItemId) -> Rc<Resolved> {
        let Some(indexed) = self.items.get(&id) else {
            return Rc::new(Resolved::default());
        };
        let module = indexed.module;
        let modules = self.std_modules;
        let scan = self.with_group(id, |group| {
            let mut scan = Scan::default();
            for item in group {
                let part = Scan::item(item, modules);
                scan.refs.extend(part.refs);
                scan.quoted.extend(part.quoted);
                scan.modifiers.extend(part.modifiers);
                for method in part.methods {
                    if !scan.methods.contains(&method) {
                        scan.methods.push(method);
                    }
                }
            }
            scan
        });
        let resolved = Rc::new(self.resolve_scan(lookup, module, Some(id), &scan));
        self.resolved.insert(id, resolved.clone());
        resolved
    }

    /// Resolves what a scan found, written in `module`. `id` is the unit it
    /// was scanned from, which does not count as a reference to itself.
    fn resolve_scan(
        &self,
        lookup: &Lookup,
        module: ModuleId,
        id: Option<ItemId>,
        scan: &Scan,
    ) -> Resolved {
        let mut resolved = Resolved::default();
        let mut unresolved: Vec<(ModuleId, String)> = Vec::new();
        let mut sort = |references: &[RawRef],
                        items: &mut Vec<ItemId>,
                        macros: &mut Vec<ItemId>,
                        track: bool,
                        this: &Self| {
            for reference in references {
                let targets = this.resolve_ref(lookup, module, reference, false);
                if track
                    && targets.is_empty()
                    && reference.path.len() == 1
                    && !Self::namespace(
                        lookup,
                        reference.definition.unwrap_or(module),
                        &reference.path[0],
                    )
                    .is_some()
                {
                    let context = reference.definition.unwrap_or(module);
                    let pair = (context, reference.path[0].clone());
                    if !unresolved.contains(&pair) {
                        unresolved.push(pair);
                    }
                }
                for target in targets {
                    if Some(target) == id {
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
        sort(
            &scan.refs,
            &mut resolved.items,
            &mut resolved.macros,
            true,
            self,
        );
        sort(
            &scan.quoted,
            &mut resolved.quoted,
            &mut resolved.quoted_macros,
            false,
            self,
        );
        resolved.unresolved = unresolved;
        for reference in &scan.modifiers {
            for target in self.resolve_ref(lookup, module, reference, true) {
                if Some(target) != id && !resolved.modifiers.contains(&target) {
                    resolved.modifiers.push(target);
                }
            }
        }
        resolved.methods = scan.methods.clone();
        // `Type.member` may name a companion function of `Type`.
        for reference in &scan.refs {
            if reference.path.len() > 1
                && let Some(member) = reference.path.last()
                && !resolved.methods.contains(member)
            {
                resolved.methods.push(member.clone());
            }
        }
        resolved
    }

    /// The implementations written in (or generated by) the unit `origin`.
    fn impls_of(&mut self, lookup: &Lookup, origin: ItemId) -> Rc<Vec<ImplEntry>> {
        if let Some(entries) = self.impls.get(&origin) {
            return entries.clone();
        }
        let module = self.items[&origin].module;
        let implementations = self.with_group(origin, |group| {
            group
                .iter()
                .filter_map(|item| match traits_plain(item) {
                    Item::TraitImplementation(implementation) => Some((
                        named_ref(&implementation.trait_name),
                        implementation
                            .type_parameters
                            .iter()
                            .flat_map(|parameter| parameter.names())
                            .map(str::to_owned)
                            .collect::<HashSet<_>>(),
                        implementation.arguments.first().cloned(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        let mut entries = Vec::new();
        for (trait_ref, parameters, self_type) in implementations {
            let traits = self
                .resolve_ref(lookup, module, &trait_ref, false)
                .into_iter()
                .filter(|target| {
                    self.items
                        .get(target)
                        .is_some_and(|item| item.kind == ItemKind::Trait)
                })
                .collect::<Vec<_>>();
            let mut heads = Vec::new();
            if let Some(self_type) = &self_type {
                collect_type_heads(self_type, &mut heads);
            }
            let blanket = matches!(
                &self_type,
                Some(Type::Named(named))
                    if named.namespace.is_none() && parameters.contains(named.name.as_str())
            );
            let selves = heads
                .into_iter()
                .filter(|head| {
                    !(head.namespace.is_none() && parameters.contains(head.name.as_str()))
                })
                .flat_map(|head| self.resolve_ref(lookup, module, &named_ref(&head), false))
                .filter(|target| {
                    self.items
                        .get(target)
                        .is_some_and(|item| item.kind == ItemKind::Type)
                })
                .collect::<Vec<_>>();
            entries.push(ImplEntry {
                origin,
                traits,
                selves,
                blanket,
            });
        }
        let entries = Rc::new(entries);
        self.impls.insert(origin, entries.clone());
        entries
    }

    /// The companion functions written in (or generated by) the unit `origin`.
    fn companions_of(&mut self, lookup: &Lookup, origin: ItemId) -> Rc<Vec<CompanionEntry>> {
        if let Some(entries) = self.companions.get(&origin) {
            return entries.clone();
        }
        let module = self.items[&origin].module;
        let members = self.with_group(origin, |group| {
            group
                .iter()
                .flat_map(companion_bindings)
                .filter_map(|binding| {
                    binding.companion_target.clone().map(|target| {
                        let generics = binding.companion_generics.as_ref();
                        let parameters = generics
                            .map(|generics| {
                                generics
                                    .parameters
                                    .iter()
                                    .flat_map(|parameter| parameter.names())
                                    .map(str::to_owned)
                                    .collect::<HashSet<_>>()
                            })
                            .unwrap_or_default();
                        let bounds = generics
                            .map(|generics| {
                                generics
                                    .trait_bounds
                                    .iter()
                                    .map(|bound| named_ref(&bound.trait_name))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        (target, parameters, bounds)
                    })
                })
                .collect::<Vec<_>>()
        });
        let mut entries = Vec::new();
        for (target, parameters, bounds) in members {
            let blanket = matches!(
                &target,
                Type::Named(named)
                    if named.namespace.is_none() && parameters.contains(named.name.as_str())
            );
            let mut heads = Vec::new();
            collect_type_heads(&target, &mut heads);
            let selves = heads
                .into_iter()
                .filter(|head| {
                    !(head.namespace.is_none() && parameters.contains(head.name.as_str()))
                })
                .flat_map(|head| self.resolve_ref(lookup, module, &named_ref(&head), false))
                .filter(|target| {
                    self.items
                        .get(target)
                        .is_some_and(|item| item.kind == ItemKind::Type)
                })
                .collect::<Vec<_>>();
            let blanket_traits = bounds
                .iter()
                .flat_map(|bound| self.resolve_ref(lookup, module, bound, false))
                .filter(|target| {
                    self.items
                        .get(target)
                        .is_some_and(|item| item.kind == ItemKind::Trait)
                })
                .collect::<Vec<_>>();
            entries.push(CompanionEntry {
                selves,
                blanket_traits,
                blanket,
            });
        }
        let entries = Rc::new(entries);
        self.companions.insert(origin, entries.clone());
        entries
    }

    /// The names, within `items`, that resolve to nothing where they are
    /// written.
    pub(super) fn unresolved_in(
        &mut self,
        lookup: &Lookup,
        items: &BTreeSet<ItemId>,
    ) -> Vec<(ModuleId, String)> {
        let mut names: Vec<(ModuleId, String)> = Vec::new();
        for item in items {
            for pair in self.resolved(lookup, *item).unresolved.iter() {
                if !names.contains(pair) {
                    names.push(pair.clone());
                }
            }
        }
        names
    }

    /// The units of `module` that are not yet expanded and can generate names:
    /// item-position macro invocations and items with modifiers.
    pub(super) fn generators_in(&self, module: ModuleId) -> Vec<ItemId> {
        let mut found: Vec<(usize, ItemId)> = self
            .items
            .iter()
            .filter(|(_, indexed)| {
                indexed.module == module && !indexed.expanded && indexed.generates
            })
            .map(|(id, indexed)| (indexed.position, *id))
            .collect();
        found.sort();
        found.into_iter().map(|(_, id)| id).collect()
    }

    /// An item of `within` that calls the macro or modifier declared by `target`.
    pub(super) fn caller_of(
        &mut self,
        lookup: &Lookup,
        within: &BTreeSet<ItemId>,
        target: ItemId,
    ) -> Option<ItemId> {
        within.iter().copied().find(|item| {
            let resolved = self.resolved(lookup, *item);
            resolved.macros.contains(&target) || resolved.modifiers.contains(&target)
        })
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
        let seed = self.resolved(lookup, start);
        self.close(lookup, Some(start), seed, is_template)
    }

    /// The closure of an expression written in `module` that compile-time code
    /// evaluates, such as a `const` initializer.
    pub(super) fn closure_of_expression(
        &mut self,
        lookup: &Lookup,
        module: ModuleId,
        expression: &Expression,
        is_template: &dyn Fn(ItemId) -> bool,
    ) -> MacroClosure {
        let scan = Scan::expression(expression, &[], self.std_modules);
        let seed = Rc::new(self.resolve_scan(lookup, module, None, &scan));
        self.close(lookup, None, seed, is_template)
    }

    /// The closure items referenced by a list of `(module, item)` pairs
    /// (such as `@compiled_only` items spliced into closure check programs).
    pub(super) fn closure_of_items(
        &mut self,
        lookup: &Lookup,
        items: &[(ModuleId, Item)],
        is_template: &dyn Fn(ItemId) -> bool,
    ) -> BTreeSet<ItemId> {
        let mut combined = Resolved::default();
        for (module, item) in items {
            let scan = Scan::item(item, self.std_modules);
            let resolved = self.resolve_scan(lookup, *module, None, &scan);
            combined.items.extend(resolved.items);
            combined.macros.extend(resolved.macros);
            combined.modifiers.extend(resolved.modifiers);
            combined.quoted.extend(resolved.quoted);
            combined.quoted_macros.extend(resolved.quoted_macros);
            combined.unresolved.extend(resolved.unresolved);
            combined.methods.extend(resolved.methods);
        }
        self.close(lookup, None, Rc::new(combined), is_template)
            .items
    }

    fn close(
        &mut self,
        lookup: &Lookup,
        start: Option<ItemId>,
        seed: Rc<Resolved>,
        is_template: &dyn Fn(ItemId) -> bool,
    ) -> MacroClosure {
        let mut closure = MacroClosure::default();
        let mut work = Vec::new();
        if let Some(start) = start {
            closure.items.insert(start);
        }
        let mut pending = vec![seed];
        // Method and member names the closure uses, which may run companion
        // functions of the types it holds.
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        let mut pending_impls = Vec::new();
        for origin in self.impl_origins.clone() {
            let group = self.impls_of(lookup, origin);
            for index in 0..group.len() {
                pending_impls.push((group.clone(), index));
            }
        }
        loop {
            while let Some(resolved) = pending
                .pop()
                .or_else(|| work.pop().map(|next| self.resolved(lookup, next)))
            {
                wanted.extend(resolved.methods.iter().cloned());
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
            // Implementations that have joined are dropped from the list, so
            // each round only looks at the ones still outside.
            pending_impls.retain(|(group, index)| {
                let entry = &group[*index];
                if closure.items.contains(&entry.origin) {
                    return false;
                }
                let has_trait = entry.traits.iter().any(|item| closure.items.contains(item));
                let has_self = entry.blanket
                    || entry.selves.is_empty()
                    || entry.selves.iter().all(|item| closure.items.contains(item));
                if has_trait && has_self {
                    closure.items.insert(entry.origin);
                    work.push(entry.origin);
                    added = true;
                    return false;
                }
                true
            });
            // A companion function joins once its name is used as a method or
            // member and the types its companion targets are in the closure. A
            // blanket companion joins once one of its bounds is.
            for name in &wanted {
                let Some(candidates) = self.methods.get(name).cloned() else {
                    continue;
                };
                for origin in candidates {
                    if closure.items.contains(&origin) {
                        continue;
                    }
                    let reachable = self.companions_of(lookup, origin).iter().any(|entry| {
                        if entry.blanket {
                            entry.blanket_traits.is_empty()
                                || entry
                                    .blanket_traits
                                    .iter()
                                    .any(|item| closure.items.contains(item))
                        } else {
                            entry.selves.iter().all(|item| closure.items.contains(item))
                        }
                    });
                    if reachable {
                        closure.items.insert(origin);
                        work.push(origin);
                        added = true;
                    }
                }
            }
            if !added && work.is_empty() {
                break;
            }
        }
        closure
    }
}

/// The companion functions an item declares: a member binding, or the
/// bindings of an `extern` block inside a companion.
fn companion_bindings(item: &Item) -> Vec<&Binding> {
    match traits_plain(item) {
        Item::Binding(binding) => vec![binding],
        Item::ExternBlock(block) => block.bindings.iter().collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .filter(|binding| binding.companion_target.is_some())
    .collect()
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
            let root =
                std::env::temp_dir().join(format!("staple-closure-{label}-{}", std::process::id()));
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
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stdlib"))
            .load_path(&fixture.root.join("main.sta"))
            .expect("fixture should load");
        let mut expander = MacroExpander::new(&program);
        load_written_items(&mut expander, &program);
        let id = expander
            .definitions
            .values()
            .find(|definition| definition.key.name == macro_name && !definition.key.modifier)
            .map(|definition| definition.declaration.syntax.id)
            .expect("the macro is declared");
        let closure = expander.macro_closure(id);
        let describe = |id: &ItemId| -> Vec<String> {
            expander
                .closure
                .group_of(*id)
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

    fn load_written_items(expander: &mut MacroExpander, program: &Program) {
        for module in program.modules() {
            expander
                .closure
                .load_raw(module.id, module.syntax.items.clone());
        }
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
    fn a_closure_follows_module_namespaces() {
        let fixture = Fixture::new("namespaces");
        fixture.write(
            "tools.sta",
            "pub mod\npub def made = (x: I32) => x\npub def other = (x: I32) => x\n",
        );
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
    fn companion_functions_follow_methods_pipes_and_members() {
        let fixture = Fixture::new("companions");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}type Box wrap I32\n\
                 type Crate wrap I32\n\
                 companion Box {{\n\
                 \x20   pub def doubled = (b: Box) => b\n\
                 \x20   pub def tripled = (b: Box) => b\n\
                 \x20   pub def unused = (b: Box) => b\n\
                 }}\n\
                 companion Crate {{\n    pub def doubled = (c: Crate) => c\n}}\n\
                 def by_pipe = (b: Box) => b |> doubled\n\
                 def by_member = (b: Box) => Box.tripled b\n\
                 macro m = value: Expr => {{ let a = by_pipe; let c = by_member; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        assert!(contains(&items, "doubled"), "{items:?}");
        assert!(contains(&items, "tripled"), "{items:?}");
        assert!(!contains(&items, "unused"), "{items:?}");
        // `Crate` is not in the closure, so its `doubled` cannot be the one a
        // method call runs.
        assert!(!contains(&items, "Crate"), "{items:?}");
        assert_eq!(
            items
                .iter()
                .filter(|item| item.as_str() == "doubled")
                .count(),
            1,
            "{items:?}"
        );
    }

    #[test]
    fn interpolation_and_indexing_reach_their_traits() {
        let fixture = Fixture::new("formatting");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}def shown = (x: I32) => \"${{x}} and ${{x:?}}\"\n\
                 def first = (xs: List<I32>, i: USize) => xs[i]\n\
                 macro m = value: Expr => {{ let a = shown; let b = first; parse_quote {{ $value }} }}\n"
            ),
        );
        let (items, _) = closure_of(&fixture, "m");
        for name in ["Display", "Debug", "Index", "MutateIndex"] {
            assert!(contains(&items, name), "{name}: {items:?}");
        }
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
        assert_eq!(
            items.iter().filter(|name| *name == "impl Tag").count(),
            1,
            "{items:?}"
        );
    }

    #[test]
    #[ignore = "prints closures of standard-library macros"]
    fn print_standard_library_closures() {
        let fixture = Fixture::new("print");
        fixture.write("main.sta", "let x: I32 = 1\n");
        for name in ["if", "when", "while", "for", "derive_eq", "typegroup"] {
            let program = ProgramLoader::new()
                .with_standard_library_root(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stdlib"),
                )
                .load_path(&fixture.root.join("main.sta"))
                .unwrap();
            let mut expander = MacroExpander::new(&program);
            load_written_items(&mut expander, &program);
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
                expander
                    .closure
                    .group_of(*id)
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
            eprintln!(
                "{name}: {} items, macros {macros:?}\n  {items:?}",
                items.len()
            );
            if let Ok(target) = std::env::var("EXPLAIN") {
                for item in closure.items.iter() {
                    let resolved = {
                        let lookup = Lookup {
                            scopes: &expander.scopes,
                            parents: &expander.parent_modules,
                            companions: &expander.companion_modules,
                        };
                        expander.closure.resolved(&lookup, *item)
                    };
                    let direct = resolved
                        .items
                        .iter()
                        .chain(&resolved.macros)
                        .filter(|t| {
                            expander.closure.items[*t]
                                .label
                                .split('+')
                                .any(|n| n == target)
                        })
                        .count();
                    if direct > 0 {
                        eprintln!("  {} -> {target}", expander.closure.items[item].label);
                    }
                }
            }
        }
    }

    #[test]
    fn quoting_macro_closure_includes_quote_runtime_and_splice_impls() {
        let fixture = Fixture::new("quote-runtime");
        fixture.write(
            "main.sta",
            &format!(
                "{HEADER}\
                 macro quote_m = x: Expr => quote {{ $x }}\n\
                 macro parse_m = x: Expr => parse_quote {{ $x }}\n"
            ),
        );
        let (quote_items, _) = closure_of(&fixture, "quote_m");
        assert!(contains(&quote_items, "__quote"), "{quote_items:?}");
        assert!(contains(&quote_items, "Sequence"), "{quote_items:?}");
        assert!(contains(&quote_items, "Splice"), "{quote_items:?}");
        assert!(contains(&quote_items, "impl Splice"), "{quote_items:?}");

        let (parse_items, _) = closure_of(&fixture, "parse_m");
        assert!(contains(&parse_items, "Parse"), "{parse_items:?}");
        assert!(contains(&parse_items, "Sequence"), "{parse_items:?}");
        assert!(contains(&parse_items, "Splice"), "{parse_items:?}");
        assert!(contains(&parse_items, "impl Splice"), "{parse_items:?}");
    }

    #[test]
    fn standard_library_has_no_macro_closure_cycles() {
        let fixture = Fixture::new("stdlib-no-cycles");
        fixture.write(
            "main.sta",
            "use std.flow.(if, when, for, while)\n\
             use std.cinterop.c_string\n\
             pub def main = () => () :: {\n\
                 let _s = c_string \"ok\"\n\
                 let _x = if (True) { 1 } else { 2 }\n\
                 let _y = when { True => 1, else => 2 }\n\
             }\n",
        );
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stdlib"))
            .load_path(&fixture.root.join("main.sta"))
            .expect("fixture should load");
        let (_, _, diagnostics) = crate::macro_expand::expand_program_checked(
            program,
            false,
            Some(crate::macro_expand::check_program::CheckMode::All),
            true,
        )
        .expect("expansion should succeed without macro closure cycles");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
