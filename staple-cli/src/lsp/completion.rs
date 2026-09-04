use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, Documentation, MarkupContent,
    MarkupKind, TextEdit,
};
use staple_compiler::*;
use staple_project::PackageId;
use staple_syntax::*;

/// Upper bound on `.sta` files scanned when building the cross-module suggestion
/// index, a guard against a pathologically large workspace stalling analysis.
const MAX_EXTERNAL_FILES: usize = 4000;

const KEYWORDS: &[&str] = &[
    "alias",
    "as",
    "break",
    "const",
    "continue",
    "def",
    "extern",
    "impl",
    "let",
    "loop",
    "macro",
    "match",
    "mod",
    "companion",
    "mut",
    "opaque",
    "package",
    "parse_quote",
    "pub",
    "quote",
    "repr",
    "resource",
    "return",
    "satisfies",
    "trait",
    "type",
    "use",
    "with",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Namespace {
    Value,
    Type,
    Trait,
    Macro,
    Module,
    Keyword,
}

#[derive(Debug, Clone)]
struct Candidate {
    available_from: usize,
    namespace: Namespace,
    item: CompletionItem,
}

#[derive(Debug, Clone)]
struct Scope {
    range: Range<usize>,
    candidates: Vec<Candidate>,
}

#[derive(Debug, Clone)]
struct ModuleIndex {
    id: ModuleId,
    range: Range<usize>,
    scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionIndex {
    modules: Vec<ModuleIndex>,
    methods: Vec<MethodSite>,
    qualifiers: Vec<MethodSite>,
    named_qualifiers: Vec<NamedQualifier>,
    named_methods: Vec<NamedQualifier>,
    /// Public items defined in other modules / dependency packages that the
    /// edited file has not imported. Offered by [`CompletionIndex::items`] with
    /// a `data` payload the LSP resolves into a `use` insertion edit.
    external: Vec<ExternalSymbol>,
}

/// A completion candidate for an item that is not in scope yet. Accepting it
/// inserts the bare name and, via `completionItem/resolve`, a matching `use`.
#[derive(Debug, Clone)]
struct ExternalSymbol {
    label: String,
    namespace: Namespace,
    kind: CompletionItemKind,
    detail: Option<String>,
    docs: Vec<String>,
    /// Dotted path of the defining module as written in a `use`, e.g. `std.io`.
    module_path: String,
}

impl ExternalSymbol {
    fn to_completion_item(&self) -> CompletionItem {
        CompletionItem {
            label: self.label.clone(),
            kind: Some(self.kind),
            detail: self.detail.clone(),
            documentation: markup_documentation(&self.docs),
            label_details: Some(CompletionItemLabelDetails {
                detail: None,
                description: Some(self.module_path.clone()),
            }),
            // Sort cross-module suggestions after everything already in scope.
            sort_text: Some(format!("~{}", self.label)),
            data: Some(serde_json::json!({
                "staple.import": self.module_path,
                "staple.item": self.label,
            })),
            ..CompletionItem::default()
        }
    }
}

#[derive(Debug, Clone)]
struct MethodSite {
    receiver_end: usize,
    items: Vec<CompletionItem>,
}

#[derive(Debug, Clone)]
struct NamedQualifier {
    module: ModuleId,
    name: String,
    items: Vec<CompletionItem>,
}

pub fn index(module: &Module, typed: &TypedModule) -> CompletionIndex {
    let mut symbol_docs = HashMap::new();
    for source in typed.resolved().program().modules() {
        collect_symbol_docs(typed, &source.syntax.items, &mut symbol_docs);
    }
    collect_symbol_docs(typed, &module.items, &mut symbol_docs);
    let mut collector = Collector {
        typed,
        index: CompletionIndex::default(),
        symbol_docs,
    };
    let entry = typed.resolved().program().entry();
    collector.module(module, entry);
    let mut index = collector.index;
    index.external = collect_external_symbols(typed);
    index
}

/// Walks module and submodule items, recording the doc comments attached to
/// each `let`/`def`/`extern` binding under its resolved symbol. Companion
/// methods and imported values both surface here, so completion candidates
/// built from a bare `DefinitionId::Symbol` can still show documentation.
fn collect_symbol_docs(
    typed: &TypedModule,
    items: &[Item],
    docs: &mut HashMap<SymbolId, Vec<String>>,
) {
    for item in items {
        collect_item_symbol_docs(typed, item, docs);
    }
}

fn collect_item_symbol_docs(
    typed: &TypedModule,
    item: &Item,
    docs: &mut HashMap<SymbolId, Vec<String>>,
) {
    match item {
        Item::Modified(value) => collect_item_symbol_docs(typed, &value.item, docs),
        Item::VisibilitySplice(value) => collect_item_symbol_docs(typed, &value.item, docs),
        Item::Submodule(value) => collect_symbol_docs(typed, &value.module.items, docs),
        Item::ExternBlock(block) => {
            for binding in &block.bindings {
                record_binding_docs(typed, binding, docs);
            }
        }
        Item::Binding(binding) => record_binding_docs(typed, binding, docs),
        _ => {}
    }
}

fn record_binding_docs(
    typed: &TypedModule,
    binding: &Binding,
    docs: &mut HashMap<SymbolId, Vec<String>>,
) {
    if binding.docs.is_empty() {
        return;
    }
    if let Some(symbol) = typed.symbol_for(binding.syntax.id) {
        docs.entry(symbol).or_insert_with(|| binding.docs.clone());
    }
}

fn markup_documentation(docs: &[String]) -> Option<Documentation> {
    if docs.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: docs.join("\n"),
    }))
}

pub fn keywords() -> Vec<CompletionItem> {
    KEYWORDS
        .iter()
        .map(|keyword| CompletionItem {
            label: (*keyword).to_owned(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..CompletionItem::default()
        })
        .collect()
}

impl CompletionIndex {
    pub fn items(&self, offset: usize) -> Vec<CompletionItem> {
        let Some(module) = self
            .modules
            .iter()
            .filter(|module| contains(&module.range, offset))
            .min_by_key(|module| module.range.end.saturating_sub(module.range.start))
        else {
            return keywords();
        };
        let mut scopes = module
            .scopes
            .iter()
            .filter(|scope| contains(&scope.range, offset))
            .collect::<Vec<_>>();
        scopes.sort_by_key(|scope| std::cmp::Reverse(scope.range.end - scope.range.start));

        let mut visible = HashMap::<(String, Namespace), CompletionItem>::new();
        for scope in scopes {
            for candidate in &scope.candidates {
                if candidate.available_from <= offset {
                    visible.insert(
                        (candidate.item.label.clone(), candidate.namespace),
                        candidate.item.clone(),
                    );
                }
            }
        }
        for item in keywords() {
            visible.insert((item.label.clone(), Namespace::Keyword), item);
        }
        let mut items = visible.into_iter().collect::<Vec<_>>();
        items.sort_by(
            |((left_label, left_namespace), left), ((right_label, right_namespace), right)| {
                left_label
                    .cmp(right_label)
                    .then(left_namespace.cmp(right_namespace))
                    .then(left.detail.cmp(&right.detail))
            },
        );
        let in_scope = items
            .iter()
            .map(|((label, namespace), _)| (label.clone(), *namespace))
            .collect::<HashSet<_>>();
        let mut result = items.into_iter().map(|(_, item)| item).collect::<Vec<_>>();
        let mut external = self
            .external
            .iter()
            .filter(|symbol| !in_scope.contains(&(symbol.label.clone(), symbol.namespace)))
            .map(ExternalSymbol::to_completion_item)
            .collect::<Vec<_>>();
        external.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then(left.detail.cmp(&right.detail))
        });
        result.extend(external);
        result
    }

    pub fn method_items(&self, receiver_end: usize) -> Vec<CompletionItem> {
        self.methods
            .iter()
            .filter(|site| site.receiver_end == receiver_end)
            .max_by_key(|site| site.items.len())
            .map(|site| site.items.clone())
            .unwrap_or_default()
    }

    pub fn qualifier_items(&self, receiver_end: usize) -> Vec<CompletionItem> {
        self.qualifiers
            .iter()
            .filter(|site| site.receiver_end == receiver_end)
            .max_by_key(|site| site.items.len())
            .map(|site| site.items.clone())
            .unwrap_or_default()
    }

    pub fn named_qualifier_items(&self, name: &str, offset: usize) -> Vec<CompletionItem> {
        let module = self
            .modules
            .iter()
            .filter(|module| contains(&module.range, offset))
            .min_by_key(|module| module.range.end.saturating_sub(module.range.start))
            .map(|module| module.id);
        self.named_qualifiers
            .iter()
            .find(|qualifier| {
                qualifier.module == module.unwrap_or(qualifier.module) && qualifier.name == name
            })
            .map(|qualifier| qualifier.items.clone())
            .unwrap_or_default()
    }

    pub fn named_method_items(&self, name: &str, offset: usize) -> Vec<CompletionItem> {
        self.named_items(&self.named_methods, name, offset)
    }

    fn named_items(
        &self,
        candidates: &[NamedQualifier],
        name: &str,
        offset: usize,
    ) -> Vec<CompletionItem> {
        let module = self
            .modules
            .iter()
            .filter(|module| contains(&module.range, offset))
            .min_by_key(|module| module.range.end.saturating_sub(module.range.start))
            .map(|module| module.id);
        candidates
            .iter()
            .find(|candidate| {
                candidate.module == module.unwrap_or(candidate.module) && candidate.name == name
            })
            .map(|candidate| candidate.items.clone())
            .unwrap_or_default()
    }
}

struct Collector<'a> {
    typed: &'a TypedModule,
    index: CompletionIndex,
    symbol_docs: HashMap<SymbolId, Vec<String>>,
}

impl Collector<'_> {
    fn module(&mut self, module: &Module, id: ModuleId) {
        let range = syntax_range(&module.syntax);
        let mut root = Scope {
            range: range.clone(),
            candidates: Vec::new(),
        };
        if let Some(definitions) = self.typed.resolved().visible_definitions(id).cloned() {
            for (name, definitions) in &definitions {
                for definition in definitions {
                    if let Some(candidate) = self.definition(name, *definition, range.start) {
                        root.candidates.push(candidate);
                    }
                }
            }
            for (name, definitions) in definitions {
                self.register_named_qualifier(id, &name, &definitions, &mut HashSet::new());
                self.register_named_method(id, &name, &definitions);
            }
        }
        self.sequential_items(&module.items, &mut root.candidates, range.start);
        for item in &module.items {
            self.register_item_method(id, item);
        }
        self.index.modules.push(ModuleIndex {
            id,
            range: range.clone(),
            scopes: vec![root],
        });
        let module_index = self.index.modules.len() - 1;
        for item in &module.items {
            self.item(item, module_index);
        }
    }

    fn register_item_method(&mut self, owner: ModuleId, item: &Item) {
        match item {
            Item::Modified(value) => self.register_item_method(owner, &value.item),
            Item::VisibilitySplice(value) => self.register_item_method(owner, &value.item),
            Item::Binding(binding) => {
                if let Some(symbol) = self.typed.symbol_for(binding.syntax.id) {
                    self.register_named_method(
                        owner,
                        &binding.name,
                        &[DefinitionId::Symbol(symbol)],
                    );
                }
            }
            _ => {}
        }
    }

    fn register_named_method(&mut self, owner: ModuleId, name: &str, definitions: &[DefinitionId]) {
        let mut items = Vec::new();
        for definition in definitions {
            let DefinitionId::Symbol(receiver) = definition else {
                continue;
            };
            let Some(ty) = self.typed.companion_type_of_symbol(*receiver) else {
                continue;
            };
            for (member, symbol) in self.typed.resolved().companion_members(ty, Some(owner)) {
                if self.typed.is_companion_method(symbol, ty)
                    && let Some(candidate) =
                        self.definition(member, DefinitionId::Symbol(symbol), 0)
                {
                    items.push(CompletionItem {
                        kind: Some(CompletionItemKind::METHOD),
                        ..candidate.item
                    });
                }
            }
        }
        items.sort_by(|left, right| left.label.cmp(&right.label));
        items.dedup_by(|left, right| left.label == right.label);
        if !items.is_empty() {
            self.index.named_methods.push(NamedQualifier {
                module: owner,
                name: name.to_owned(),
                items,
            });
        }
    }

    fn register_named_qualifier(
        &mut self,
        owner: ModuleId,
        name: &str,
        definitions: &[DefinitionId],
        visited: &mut HashSet<ModuleId>,
    ) {
        let mut items = Vec::new();
        for definition in definitions {
            match *definition {
                DefinitionId::Type(ty) => items.extend(
                    self.typed
                        .resolved()
                        .companion_members(ty, Some(owner))
                        .into_iter()
                        .filter_map(|(member, symbol)| {
                            self.definition(member, DefinitionId::Symbol(symbol), 0)
                                .map(|candidate| candidate.item)
                        }),
                ),
                DefinitionId::Trait(trait_id) => items.extend(
                    self.typed
                        .resolved()
                        .trait_methods(trait_id)
                        .into_iter()
                        .filter_map(|method| {
                            let member = self.typed.resolved().trait_method(method)?;
                            self.definition(&member.name, DefinitionId::TraitMethod(method), 0)
                                .map(|candidate| candidate.item)
                        }),
                ),
                DefinitionId::Module(module) => {
                    if let Some(exports) =
                        self.typed.resolved().exported_definitions(module).cloned()
                    {
                        self.add_module_items(module, &mut items);
                        if visited.insert(module) {
                            for (child, child_definitions) in exports {
                                self.register_named_qualifier(
                                    owner,
                                    &format!("{name}.{child}"),
                                    &child_definitions,
                                    visited,
                                );
                            }
                            visited.remove(&module);
                        }
                    }
                }
                _ => {}
            }
        }
        if !items.is_empty() {
            items.sort_by(|left, right| left.label.cmp(&right.label));
            items.dedup_by(|left, right| left.label == right.label && left.kind == right.kind);
            self.index.named_qualifiers.push(NamedQualifier {
                module: owner,
                name: name.to_owned(),
                items,
            });
        }
    }

    fn sequential_items(&self, items: &[Item], candidates: &mut Vec<Candidate>, start: usize) {
        for item in items {
            match item {
                Item::Modified(value) => self.sequential_item(&value.item, candidates, start),
                Item::VisibilitySplice(value) => {
                    self.sequential_item(&value.item, candidates, start)
                }
                other => self.sequential_item(other, candidates, start),
            }
        }
    }

    fn sequential_item(&self, item: &Item, candidates: &mut Vec<Candidate>, start: usize) {
        match item {
            item @ (Item::Binding(_) | Item::PatternBinding(_)) => {
                self.item_declarations(item, candidates, start)
            }
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    if let Some(candidate) = self.binding_candidate(binding, start) {
                        candidates.push(candidate);
                    }
                }
            }
            _ => {}
        }
    }

    fn item_declarations(&self, item: &Item, candidates: &mut Vec<Candidate>, scope_start: usize) {
        match item {
            Item::Modified(value) => self.item_declarations(&value.item, candidates, scope_start),
            Item::Binding(binding) => {
                let available = if matches!(binding.kind, BindingKind::Def | BindingKind::Const) {
                    scope_start
                } else {
                    syntax_range(&binding.syntax).end
                };
                if let Some(candidate) = self.binding_candidate(binding, available) {
                    candidates.push(candidate);
                }
            }
            Item::PatternBinding(binding) => {
                let available = syntax_range(&binding.syntax).end;
                self.pattern_candidates(&binding.pattern, available, candidates);
            }
            _ => {}
        }
    }

    fn item(&mut self, item: &Item, module_index: usize) {
        match item {
            Item::Modified(value) => {
                for modifier in &value.modifiers {
                    if let Some(expression) = modifier
                        .argument
                        .as_ref()
                        .and_then(|argument| argument.expression.as_ref())
                    {
                        self.expression(expression, module_index);
                    }
                }
                self.item(&value.item, module_index);
            }
            Item::VisibilityMacroInvocation(value) => {
                for modifier in &value.modifiers {
                    if let Some(expression) = modifier
                        .argument
                        .as_ref()
                        .and_then(|argument| argument.expression.as_ref())
                    {
                        self.expression(expression, module_index);
                    }
                }
                self.expression(&value.expression, module_index)
            }
            Item::VisibilitySplice(value) => self.item(&value.item, module_index),
            Item::Submodule(value) => {
                if let Some(id) = self
                    .typed
                    .resolved()
                    .program()
                    .child_module(value.syntax.id)
                {
                    self.module(&value.module, id);
                }
            }
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    self.binding(binding, module_index);
                }
            }
            Item::MacroDeclaration(value) => {
                if let Some(expression) = &value.value {
                    self.expression(expression, module_index);
                }
            }
            Item::TraitDeclaration(value) => {
                for member in &value.members {
                    if let Some(default) = &member.default {
                        self.expression(default, module_index);
                    }
                }
            }
            Item::TraitImplementation(value) => {
                for member in &value.members {
                    self.expression(&member.value, module_index);
                }
            }
            item @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.block_item(item, module_index),
            Item::RepeatedItemSplice(_) | Item::UseDeclaration(_) | Item::TypeDeclaration(_) => {}
        }
    }

    fn binding(&mut self, binding: &Binding, module_index: usize) {
        if !binding.type_parameters.is_empty() {
            let mut candidates = Vec::new();
            for parameter in &binding.type_parameters {
                self.type_parameter_candidates(parameter, &mut candidates);
            }
            self.index.modules[module_index].scopes.push(Scope {
                range: syntax_range(&binding.syntax),
                candidates,
            });
        }
        if let Some(value) = &binding.value {
            self.expression(value, module_index);
        }
    }

    fn block_item(&mut self, item: &Item, module_index: usize) {
        match item {
            Item::Binding(value) => self.binding(value, module_index),
            Item::PatternBinding(value) => self.expression(&value.value, module_index),
            Item::Assignment(value) => {
                self.expression(&value.target, module_index);
                self.expression(&value.value, module_index);
            }
            Item::Return(value) => self.expression(&value.value, module_index),
            Item::Break(value) => {
                if let Some(expression) = &value.value {
                    self.expression(expression, module_index);
                }
            }
            Item::Expression(value) => self.expression(value, module_index),
            Item::Continue(_) => {}
            Item::Submodule(value) => {
                if let Some(id) = self
                    .typed
                    .resolved()
                    .program()
                    .child_module(value.syntax.id)
                {
                    self.module(&value.module, id);
                }
            }
            Item::TypeDeclaration(_) => {}
            Item::UseDeclaration(_) => {}
            _ => {}
        }
    }

    fn expression(&mut self, expression: &Expression, module_index: usize) {
        self.qualified_items(expression, module_index);
        if let Some(ty) = self.typed.companion_type_of_expression(expression) {
            let accessing_module = Some(self.index.modules[module_index].id);
            let mut items = self
                .typed
                .resolved()
                .companion_members(ty, accessing_module)
                .into_iter()
                .filter_map(|(name, symbol)| {
                    if !self.typed.is_companion_method(symbol, ty) {
                        return None;
                    }
                    self.definition(name, DefinitionId::Symbol(symbol), 0)
                        .map(|candidate| CompletionItem {
                            kind: Some(CompletionItemKind::METHOD),
                            ..candidate.item
                        })
                })
                .collect::<Vec<_>>();
            items.sort_by(|left, right| left.label.cmp(&right.label));
            if !items.is_empty() {
                self.index.methods.push(MethodSite {
                    receiver_end: syntax_range(expression.syntax()).end,
                    items,
                });
            }
        }
        match expression {
            Expression::Function(value) => {
                let mut candidates = Vec::new();
                self.pattern_candidates(
                    &value.pattern,
                    syntax_range(value.body.syntax()).start,
                    &mut candidates,
                );
                self.index.modules[module_index].scopes.push(Scope {
                    range: syntax_range(&value.syntax),
                    candidates,
                });
                self.expression(&value.body, module_index);
            }
            Expression::Match(value) => {
                self.expression(&value.subject, module_index);
                for arm in &value.arms {
                    let mut candidates = Vec::new();
                    self.pattern_candidates(
                        &arm.pattern,
                        syntax_range(arm.body.syntax()).start,
                        &mut candidates,
                    );
                    self.index.modules[module_index].scopes.push(Scope {
                        range: syntax_range(&arm.syntax),
                        candidates,
                    });
                    self.expression(&arm.body, module_index);
                }
            }
            Expression::Loop(value) => self.block(&value.body, module_index),
            Expression::With(value) => {
                self.expression(&value.value, module_index);
                self.block(&value.body, module_index);
            }
            Expression::Block(value) => self.block(value, module_index),
            Expression::Satisfies(value) => self.expression(&value.value, module_index),
            Expression::Product(value) => {
                for element in &value.elements {
                    self.expression(&element.value, module_index);
                }
            }
            Expression::RepeatedProduct(value) => {
                self.expression(&value.value, module_index);
                self.expression(&value.count, module_index);
            }
            Expression::StringTemplate(value) => {
                for part in &value.parts {
                    if let StringTemplatePart::Interpolation(value) = part {
                        self.expression(&value.expression, module_index);
                    }
                }
            }
            Expression::Call(value) => {
                self.expression(&value.callee, module_index);
                self.expression(&value.argument, module_index);
            }
            Expression::Access(value) => self.expression(&value.value, module_index),
            Expression::Index(value) => {
                self.expression(&value.value, module_index);
                self.expression(&value.index, module_index);
            }
            Expression::Binary(value) => {
                self.expression(&value.left, module_index);
                self.expression(&value.right, module_index);
            }
            Expression::Logical(value) => {
                self.expression(&value.left, module_index);
                self.expression(&value.right, module_index);
            }
            Expression::Quote(value) => match &value.template {
                QuoteTemplate::Expression(value) => self.expression(value, module_index),
                QuoteTemplate::Item(value) => self.item(value, module_index),
                QuoteTemplate::Items(values) => {
                    for value in values {
                        self.item(value, module_index);
                    }
                }
                QuoteTemplate::Raw => {}
            },
            Expression::Resource(_)
            | Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn qualified_items(&mut self, expression: &Expression, module_index: usize) {
        let resolved = self.typed.resolved();
        let accessing_module = Some(self.index.modules[module_index].id);
        let mut items = Vec::new();
        let definitions = resolved.definitions_for(expression.syntax().id);
        if let Some(module) = resolved.namespace_for(expression.syntax().id) {
            self.add_module_items(module, &mut items);
        }
        for definition in definitions {
            match definition {
                DefinitionId::Type(ty) => items.extend(
                    resolved
                        .companion_members(ty, accessing_module)
                        .into_iter()
                        .filter_map(|(name, symbol)| {
                            self.definition(name, DefinitionId::Symbol(symbol), 0)
                                .map(|candidate| candidate.item)
                        }),
                ),
                DefinitionId::Trait(trait_id) => {
                    items.extend(resolved.trait_methods(trait_id).into_iter().filter_map(
                        |method| {
                            let member = resolved.trait_method(method)?;
                            self.definition(&member.name, DefinitionId::TraitMethod(method), 0)
                                .map(|candidate| candidate.item)
                        },
                    ))
                }
                DefinitionId::Module(module) => self.add_module_items(module, &mut items),
                _ => {}
            }
        }
        items.sort_by(|left, right| left.label.cmp(&right.label));
        items.dedup_by(|left, right| left.label == right.label && left.kind == right.kind);
        if !items.is_empty() {
            self.index.qualifiers.push(MethodSite {
                receiver_end: syntax_range(expression.syntax()).end,
                items,
            });
        }
    }

    fn add_module_items(&self, module: ModuleId, items: &mut Vec<CompletionItem>) {
        if let Some(exports) = self.typed.resolved().exported_definitions(module) {
            for (name, definitions) in exports {
                items.extend(definitions.iter().filter_map(|definition| {
                    self.definition(name, *definition, 0)
                        .map(|candidate| candidate.item)
                }));
            }
        }
    }

    fn block(&mut self, block: &BlockExpression, module_index: usize) {
        let range = syntax_range(&block.syntax);
        let mut candidates = Vec::new();
        for item in &block.items {
            self.item_declarations(item, &mut candidates, range.start);
        }
        self.index.modules[module_index]
            .scopes
            .push(Scope { range, candidates });
        for item in &block.items {
            self.item(item, module_index);
        }
    }

    fn binding_candidate(&self, binding: &Binding, available_from: usize) -> Option<Candidate> {
        let symbol = self.typed.symbol_for(binding.syntax.id)?;
        let mut candidate =
            self.definition(&binding.name, DefinitionId::Symbol(symbol), available_from)?;
        let prefix = binding.declaration_prefix();
        if let Some(detail) = candidate.item.detail.take() {
            candidate.item.detail = Some(format!("{prefix} {}: {detail}", binding.name));
        }
        if candidate.item.documentation.is_none() {
            candidate.item.documentation = markup_documentation(&binding.docs);
        }
        Some(candidate)
    }

    fn pattern_candidates(
        &self,
        pattern: &Pattern,
        available_from: usize,
        candidates: &mut Vec<Candidate>,
    ) {
        match pattern {
            Pattern::At(at) => {
                self.pattern_candidates(
                    &Pattern::Binding(at.binding.as_ref().clone()),
                    available_from,
                    candidates,
                );
                self.pattern_candidates(&at.pattern, available_from, candidates);
            }
            Pattern::Binding(value) => {
                if let Some(symbol) = self.typed.symbol_for(value.syntax.id)
                    && let Some(mut candidate) =
                        self.definition(&value.name, DefinitionId::Symbol(symbol), available_from)
                {
                    if let Some(detail) = candidate.item.detail.take() {
                        candidate.item.detail = Some(format!("{}: {detail}", value.name));
                    }
                    candidates.push(candidate);
                }
            }
            Pattern::Product(value) => {
                for element in &value.elements {
                    self.pattern_candidates(element, available_from, candidates);
                }
            }
            Pattern::Nominal(value) => {
                self.pattern_candidates(&value.argument, available_from, candidates)
            }
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }

    fn type_parameter_candidates(
        &self,
        parameter: &TypeParameterPattern,
        candidates: &mut Vec<Candidate>,
    ) {
        match parameter {
            TypeParameterPattern::Binding(value) => candidates.push(Candidate {
                available_from: syntax_range(&value.syntax).end,
                namespace: Namespace::Type,
                item: CompletionItem {
                    label: value.name.clone(),
                    kind: Some(CompletionItemKind::TYPE_PARAMETER),
                    detail: Some(format!("type parameter {}", value.name)),
                    ..CompletionItem::default()
                },
            }),
            TypeParameterPattern::Effect(_) => {}
            TypeParameterPattern::Product(value) => {
                for element in &value.elements {
                    self.type_parameter_candidates(element, candidates);
                }
            }
            TypeParameterPattern::Splice(_) => {}
        }
    }

    fn definition(
        &self,
        name: &str,
        definition: DefinitionId,
        available_from: usize,
    ) -> Option<Candidate> {
        let resolved = self.typed.resolved();
        let (namespace, kind, detail) = match definition {
            DefinitionId::Symbol(symbol) => {
                let ty = self.typed.type_of_symbol(symbol)?;
                let kind = if matches!(ty, CheckedType::Function(_)) {
                    CompletionItemKind::FUNCTION
                } else {
                    CompletionItemKind::VARIABLE
                };
                (Namespace::Value, kind, Some(display_type(self.typed, ty)))
            }
            DefinitionId::Type(id) => {
                let declaration = resolved.type_declarations().get(&id)?;
                (
                    Namespace::Type,
                    CompletionItemKind::CLASS,
                    Some(format!("type {}", declaration.name)),
                )
            }
            DefinitionId::TypeParameter(_) => (
                Namespace::Type,
                CompletionItemKind::TYPE_PARAMETER,
                Some(format!("type parameter {name}")),
            ),
            DefinitionId::Trait(id) => {
                let declaration = &resolved.traits().get(&id)?.declaration;
                (
                    Namespace::Trait,
                    CompletionItemKind::INTERFACE,
                    Some(format!("trait {}", declaration.name)),
                )
            }
            DefinitionId::TraitMethod(id) => {
                let method = resolved.trait_method(id)?;
                (
                    Namespace::Value,
                    CompletionItemKind::METHOD,
                    Some(format!(
                        "{}: {}",
                        method.name,
                        method.annotation.syntax().text().trim()
                    )),
                )
            }
            DefinitionId::Macro(id) => {
                let macro_ = resolved.macro_for(id)?;
                (
                    Namespace::Macro,
                    CompletionItemKind::FUNCTION,
                    Some(format!("macro {}: {}", macro_.name, macro_.signature)),
                )
            }
            DefinitionId::Module(_) => (
                Namespace::Module,
                CompletionItemKind::MODULE,
                Some(format!("module {name}")),
            ),
            DefinitionId::CompileTime(syntax) => {
                let info = resolved.compile_time_binding_for(syntax)?;
                let kind = match info.kind {
                    CompileTimeBindingKind::Helper => CompletionItemKind::FUNCTION,
                    CompileTimeBindingKind::Builtin => CompletionItemKind::CONSTRUCTOR,
                    _ => CompletionItemKind::VARIABLE,
                };
                (Namespace::Value, kind, info.type_display.clone())
            }
        };
        let docs = match definition {
            DefinitionId::Symbol(symbol) => {
                self.symbol_docs.get(&symbol).cloned().unwrap_or_default()
            }
            DefinitionId::Type(id) => resolved
                .type_declarations()
                .get(&id)
                .map(|declaration| declaration.docs.clone())
                .unwrap_or_default(),
            DefinitionId::Trait(id) => resolved
                .traits()
                .get(&id)
                .map(|resolved| resolved.declaration.docs.clone())
                .unwrap_or_default(),
            DefinitionId::TraitMethod(id) => resolved
                .trait_method(id)
                .map(|method| method.docs.clone())
                .unwrap_or_default(),
            DefinitionId::Macro(id) => resolved
                .macro_for(id)
                .map(|macro_| macro_.docs.clone())
                .unwrap_or_default(),
            DefinitionId::Module(id) => resolved.program().module(id).syntax.docs.clone(),
            DefinitionId::TypeParameter(_) | DefinitionId::CompileTime(_) => Vec::new(),
        };
        Some(Candidate {
            available_from,
            namespace,
            item: CompletionItem {
                label: name.to_owned(),
                kind: Some(kind),
                detail,
                documentation: markup_documentation(&docs),
                ..CompletionItem::default()
            },
        })
    }
}

fn display_type(typed: &TypedModule, ty: &CheckedType) -> String {
    let resolved = typed.resolved();
    let mut names = resolved
        .type_declarations()
        .iter()
        .filter_map(|(id, declaration)| {
            let internal = resolved.type_name(*id)?;
            (internal != declaration.name).then_some((internal, declaration.name.as_str()))
        })
        .collect::<Vec<_>>();
    names.sort_unstable_by_key(|(internal, _)| std::cmp::Reverse(internal.len()));
    let mut displayed = ty.to_string();
    for (internal, source) in names {
        displayed = displayed.replace(internal, source);
    }
    displayed
}

fn syntax_range(syntax: &Syntax) -> Range<usize> {
    syntax.span.to_range()
}

fn contains(range: &Range<usize>, offset: usize) -> bool {
    range.start <= offset && offset <= range.end
}

/// Scans every `.sta` file in the package graph (workspace packages, their
/// dependencies, and the standard library) for public declarations that the
/// edited file could import. Only the AST is read — no name resolution — so a
/// broken file elsewhere in the workspace is skipped rather than failing the
/// whole analysis.
fn collect_external_symbols(typed: &TypedModule) -> Vec<ExternalSymbol> {
    let resolved = typed.resolved();
    let program = resolved.program();
    let Some(graph) = program.package_graph() else {
        return Vec::new();
    };
    let entry = program.entry();
    let current_package = program.package_of(entry);
    let current_path = canonical(&program.module(entry).path);

    // Names already reachable in the edited module (locals, prelude, existing
    // imports) — never offer these as cross-module suggestions.
    let visible = resolved
        .visible_definitions(entry)
        .map(|definitions| definitions.keys().cloned().collect::<HashSet<_>>())
        .unwrap_or_default();

    let mut symbols = Vec::new();
    let mut seen_files = HashSet::new();
    let mut budget = MAX_EXTERNAL_FILES;
    for index in 0..graph.packages.len() {
        let target_package = PackageId(index);
        let package = &graph.packages[index];

        // The leading segment of the `use` path: nothing for a file in the
        // edited file's own package (imported by a source-root-relative path),
        // otherwise the alias this package declares for the dependency.
        let base_segment = if current_package == Some(target_package) {
            None
        } else {
            let alias = current_package.and_then(|current| {
                graph
                    .package(current)
                    .dependencies
                    .iter()
                    .find(|dependency| dependency.package == target_package)
                    .map(|dependency| dependency.alias.clone())
            });
            match alias {
                Some(alias) => Some(alias),
                // A lone file with no real package can still import `std`.
                None if current_package.is_none() && package.name == "std" => {
                    Some("std".to_owned())
                }
                None => continue,
            }
        };

        let source_root = canonical(package.source_root());
        let mut files = Vec::new();
        collect_sta_files(&source_root, &mut files);
        files.sort();
        for file in files {
            if budget == 0 {
                return symbols;
            }
            let file = canonical(&file);
            if file == current_path || !seen_files.insert(file.clone()) {
                continue;
            }
            budget -= 1;
            let Some(module_path) =
                dotted_module_path(base_segment.as_deref(), &source_root, &file)
            else {
                continue;
            };
            let Ok(source) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(parsed) = parse(&source) else {
                continue;
            };
            let mut scan = ExternalScan {
                module_path: &module_path,
                same_package: current_package == Some(target_package),
                visible: &visible,
                symbols: &mut symbols,
            };
            for item in &parsed.items {
                scan.item(item);
            }
        }
    }
    symbols
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

fn collect_sta_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sta_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "sta") {
            files.push(path);
        }
    }
}

/// The dotted `use` path of a file, e.g. `std.io` or `tools.text`. Mirrors
/// `staple_compiler::Program::module_dotted_name` but works from a file path
/// alone, for modules that were never loaded. Returns `None` when the file is
/// the package root itself (nothing to import by module path) or lies outside
/// the source root.
fn dotted_module_path(
    base_segment: Option<&str>,
    source_root: &Path,
    file: &Path,
) -> Option<String> {
    let relative = file.strip_prefix(source_root).ok()?.with_extension("");
    let mut segments = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    // A `root.sta` directly in the source root names the package, not a
    // `.root` submodule.
    if segments == ["root"] {
        segments.clear();
    }
    let mut path = base_segment
        .map(str::to_owned)
        .into_iter()
        .collect::<Vec<_>>();
    path.extend(segments);
    (!path.is_empty()).then(|| path.join("."))
}

struct ExternalScan<'a> {
    module_path: &'a str,
    same_package: bool,
    visible: &'a HashSet<String>,
    symbols: &'a mut Vec<ExternalSymbol>,
}

impl ExternalScan<'_> {
    fn item(&mut self, item: &Item) {
        match item {
            Item::Modified(value) => self.item(&value.item),
            Item::VisibilitySplice(value) => self.item(&value.item),
            Item::TypeDeclaration(declaration) => self.push(
                declaration.visibility,
                &declaration.name,
                Namespace::Type,
                CompletionItemKind::CLASS,
                Some(format!("type {}", declaration.name)),
                &declaration.docs,
            ),
            Item::TraitDeclaration(declaration) => self.push(
                declaration.visibility,
                &declaration.name,
                Namespace::Trait,
                CompletionItemKind::INTERFACE,
                Some(format!("trait {}", declaration.name)),
                &declaration.docs,
            ),
            Item::MacroDeclaration(declaration) => {
                let detail = match &declaration.annotation {
                    Some(annotation) => format!(
                        "macro {}: {}",
                        declaration.name,
                        annotation.syntax().text().trim()
                    ),
                    None => format!("macro {}", declaration.name),
                };
                self.push(
                    declaration.visibility,
                    &declaration.name,
                    Namespace::Macro,
                    CompletionItemKind::FUNCTION,
                    Some(detail),
                    &declaration.docs,
                );
            }
            Item::Binding(binding) => self.binding(binding.visibility, binding),
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    self.binding(block.visibility, binding);
                }
            }
            _ => {}
        }
    }

    fn binding(&mut self, visibility: Visibility, binding: &Binding) {
        let kind = if matches!(binding.kind, BindingKind::Def) {
            CompletionItemKind::FUNCTION
        } else {
            CompletionItemKind::VARIABLE
        };
        let name = &binding.name;
        let detail = match (&binding.annotation, binding.external) {
            (Some(annotation), true) => {
                format!("{}: {}", name, annotation.syntax().text().trim())
            }
            (Some(annotation), false) => format!(
                "{} {}: {}",
                binding.declaration_prefix(),
                name,
                annotation.syntax().text().trim()
            ),
            (None, true) => name.clone(),
            (None, false) => format!("{} {}", binding.declaration_prefix(), name),
        };
        self.push(
            visibility,
            name,
            Namespace::Value,
            kind,
            Some(detail),
            &binding.docs,
        );
    }

    fn push(
        &mut self,
        visibility: Visibility,
        name: &str,
        namespace: Namespace,
        kind: CompletionItemKind,
        detail: Option<String>,
        docs: &[String],
    ) {
        let importable = match visibility {
            Visibility::Public => true,
            Visibility::Package => self.same_package,
            Visibility::Private => false,
        };
        if !importable || self.visible.contains(name) {
            return;
        }
        self.symbols.push(ExternalSymbol {
            label: name.to_owned(),
            namespace,
            kind,
            detail,
            docs: docs.to_vec(),
            module_path: self.module_path.to_owned(),
        });
    }
}

/// Computes the `use`-declaration edit for an accepted cross-module completion.
/// Merges into an existing `use` for the same module when possible, otherwise
/// inserts a fresh line. `None` means "no edit" — either the item is already
/// imported or the buffer could not be parsed.
pub fn resolve_import_edits(text: &str, module_path: &str, item: &str) -> Option<Vec<TextEdit>> {
    let module = parse(text).ok()?;

    for top_level in &module.items {
        let Some(declaration) = top_level_use(top_level) else {
            continue;
        };
        let span = declaration.syntax.span.to_range();
        let declaration_text = text.get(span.clone())?;
        match &declaration.kind {
            UseKind::Selected(names) => {
                if declaration.path.join(".") != module_path {
                    continue;
                }
                if names.iter().any(|name| name == item) {
                    return Some(Vec::new());
                }
                let close = span.start + declaration_text.rfind(')')?;
                return Some(vec![insert_edit(text, close, format!(", {item}"))]);
            }
            UseKind::Glob => {
                if declaration.path.join(".") == module_path {
                    return Some(Vec::new());
                }
            }
            UseKind::Dotted => {
                if let Some((last, head)) = declaration.path.split_last()
                    && !head.is_empty()
                    && head.join(".") == module_path
                {
                    if last == item {
                        return Some(Vec::new());
                    }
                    let new_text = format!(
                        "{}use {}.({}, {})",
                        visibility_prefix(declaration.visibility),
                        module_path,
                        last,
                        item
                    );
                    return Some(vec![replace_edit(text, span, new_text)]);
                }
            }
            UseKind::Namespace | UseKind::Renamed { .. } => {}
        }
    }

    let insertion = new_use_offset(&module);
    let new_text = if insertion == 0 {
        format!("use {module_path}.{item}\n")
    } else {
        format!("\nuse {module_path}.{item}")
    };
    Some(vec![insert_edit(text, insertion, new_text)])
}

fn top_level_use(item: &Item) -> Option<&UseDeclaration> {
    match item {
        Item::UseDeclaration(declaration) => Some(declaration),
        Item::Modified(value) => top_level_use(&value.item),
        Item::VisibilitySplice(value) => top_level_use(&value.item),
        _ => None,
    }
}

fn visibility_prefix(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "pub ",
        Visibility::Package => "pub(package) ",
        Visibility::Private => "",
    }
}

/// Byte offset for a newly inserted `use` line: after the last top-level `use`,
/// else after the bare `mod` header, else the start of the file.
fn new_use_offset(module: &Module) -> usize {
    let last_use_end = module
        .items
        .iter()
        .filter_map(top_level_use)
        .map(|declaration| declaration.syntax.span.to_range().end)
        .max();
    if let Some(end) = last_use_end {
        return end;
    }
    if let Some(declaration) = &module.declaration_syntax {
        return declaration.span.to_range().end;
    }
    0
}

fn insert_edit(text: &str, offset: usize, new_text: String) -> TextEdit {
    let position = offset_to_position(text, offset);
    TextEdit {
        range: lsp_types::Range::new(position, position),
        new_text,
    }
}

fn replace_edit(text: &str, range: Range<usize>, new_text: String) -> TextEdit {
    TextEdit {
        range: lsp_types::Range::new(
            offset_to_position(text, range.start),
            offset_to_position(text, range.end),
        ),
        new_text,
    }
}

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let (line, character) = crate::lsp::semantic::position(text, offset.min(text.len()));
    lsp_types::Position::new(line, character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn completes_keywords_module_names_imports_and_lexical_bindings() {
        let root =
            std::env::temp_dir().join(format!("staple-completion-scopes-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            "pub mod\npub let imported = 1\n",
        )
        .unwrap();
        let source = concat!(
            "use dependency.*\n",
            "def outer = parameter: I32 => {\n",
            "    0\n",
            "    let local = 1\n",
            "    def nested = () => 1\n",
            "    local + nested ()\n",
            "}\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let index = index(&module, &typed);

        let before_local = source.find("    0\n").unwrap() + 4;
        let before = index.items(before_local);
        for expected in ["def", "I32", "imported", "nested", "outer", "parameter"] {
            assert!(
                before.iter().any(|item| item.label == expected),
                "missing {expected} in {before:?}"
            );
        }
        assert!(!before.iter().any(|item| item.label == "local"));

        let after_local = source.find("    local +").unwrap() + 5;
        assert!(
            index
                .items(after_local)
                .iter()
                .any(|item| item.label == "local")
        );

        let outside = index.items(source.len());
        assert!(!outside.iter().any(|item| item.label == "parameter"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signal_binding_completion_detail_shows_let_signal_prefix() {
        let source = "let signal count = 0\ncount = 1\n";
        let path = std::env::temp_dir().join("staple-completion-signal.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let items = index(&module, &typed).items(source.rfind("count").unwrap());
        let counts = items
            .iter()
            .filter(|item| item.label == "count")
            .collect::<Vec<_>>();
        assert_eq!(counts.len(), 1, "items: {items:?}");
        assert_eq!(counts[0].detail.as_deref(), Some("let signal count: I32"));
    }

    #[test]
    fn inner_bindings_shadow_outer_values() {
        let source = "let value = 1\ndef choose = value: I32 => value\n";
        let path = std::env::temp_dir().join("staple-completion-shadow.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let items = index(&module, &typed).items(source.rfind("value").unwrap());
        let values = items
            .iter()
            .filter(|item| item.label == "value")
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1, "items: {items:?}");
        assert_eq!(values[0].detail.as_deref(), Some("value: I32"));
    }

    #[test]
    fn completes_at_pattern_aliases_and_nested_bindings() {
        let source = concat!(
            "def sum = pair@(left: I32, right: I32) => {\n",
            "    left + right\n",
            "}\n",
        );
        let path = std::env::temp_dir().join("staple-completion-at-patterns.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let index = index(&module, &typed);
        let offset = source.find("    left +").unwrap() + 4;
        let items = index.items(offset);

        for expected in ["pair", "left", "right"] {
            assert!(
                items.iter().any(|item| item.label == expected),
                "missing {expected} in {items:?}"
            );
        }
    }

    #[test]
    fn completes_bare_trait_function() {
        let source = concat!(
            "type Wrapper = I32\n",
            "impl ToString Wrapper { def to_string = value => \"\" }\n",
            "def f: Wrapper -> String = to_string\n",
        );
        let path = std::env::temp_dir().join("staple-completion-trait-bare.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let items = index(&module, &typed).items(source.len());

        let matches = items
            .iter()
            .filter(|item| item.label == "to_string")
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "items: {items:?}");
        assert_eq!(matches[0].kind, Some(CompletionItemKind::METHOD));
    }

    #[test]
    fn maps_imported_definition_namespaces_to_completion_kinds() {
        let root =
            std::env::temp_dir().join(format!("staple-completion-kinds-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            concat!(
                "pub mod\n",
                "pub def callable = () => 1\n",
                "pub type alias Number = I32\n",
                "pub trait Printable T {}\n",
                "pub macro identity = value => parse_quote { $value }\n",
            ),
        )
        .unwrap();
        let source = concat!(
            "use dependency.(callable, Number, Printable, identity)\n",
            "use dependency\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let items = index(&module, &typed).items(source.len());

        for (label, kind) in [
            ("callable", CompletionItemKind::FUNCTION),
            ("Number", CompletionItemKind::CLASS),
            ("Printable", CompletionItemKind::INTERFACE),
            ("identity", CompletionItemKind::FUNCTION),
            ("dependency", CompletionItemKind::MODULE),
        ] {
            assert!(
                items
                    .iter()
                    .any(|item| item.label == label && item.kind == Some(kind)),
                "missing {label} in {items:?}"
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completes_accessible_companion_methods_for_a_typed_receiver() {
        let source = concat!(
            "def exercise = () => {\n",
            "let mut numbers: List I32 = List.new ()\n",
            "numbers\n",
            "}\n",
        );
        let path = std::env::temp_dir().join("staple-completion-methods.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let receiver_end = source.rfind("numbers").unwrap() + "numbers".len();
        let items = index(&module, &typed).method_items(receiver_end);

        assert!(
            items.iter().any(|item| {
                item.label == "push" && item.kind == Some(CompletionItemKind::METHOD)
            }),
            "items: {items:?}"
        );
        assert!(!items.iter().any(|item| item.label == "new"));
        assert!(!items.iter().any(|item| item.label == "grow"));
    }

    #[test]
    fn completes_qualified_modules_companions_and_trait_members() {
        let source = concat!(
            "trait Local T { render: T -> String }\n",
            "impl Local I32 { def render = value => \"\" }\n",
            "let print = std.io.println\n",
            "let list: List I32 = List.new ()\n",
            "let render = Local.render\n",
        );
        let path = std::env::temp_dir().join("staple-completion-qualified.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let index = index(&module, &typed);

        for (receiver, expected) in [("std.io", "println"), ("List", "new"), ("Local", "render")] {
            let receiver_end = source.rfind(receiver).unwrap() + receiver.len();
            let items = index.qualifier_items(receiver_end);
            assert!(
                items.iter().any(|item| item.label == expected),
                "missing {expected} after {receiver}. in {items:?}"
            );
        }
    }

    #[test]
    fn completes_a_typed_qualifier_when_the_indexed_source_is_empty() {
        let source = "";
        let path = std::env::temp_dir().join("staple-completion-empty-qualified.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let index = index(&module, &typed);

        assert!(
            index
                .named_qualifier_items("List", 0)
                .iter()
                .any(|item| item.label == "singleton")
        );
        assert!(
            index
                .named_qualifier_items("ToString", 0)
                .iter()
                .any(|item| item.label == "to_string")
        );
    }

    #[test]
    fn completion_items_carry_doc_comments() {
        let root =
            std::env::temp_dir().join(format!("staple-completion-docs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            concat!(
                "pub mod\n",
                "/// Adds one to its argument.\n",
                "pub def bump = value: I32 => value\n",
            ),
        )
        .unwrap();
        let source = concat!(
            "use dependency.bump\n",
            "/// The magic number.\n",
            "def answer: I32 = 42\n",
            "answer\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let items = index(&module, &typed).items(source.rfind("answer").unwrap());

        let documentation = |label: &str| {
            items
                .iter()
                .find(|item| item.label == label)
                .and_then(|item| item.documentation.clone())
        };
        assert_eq!(
            documentation("answer"),
            Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: " The magic number.".to_owned(),
            }))
        );
        assert_eq!(
            documentation("bump"),
            Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: " Adds one to its argument.".to_owned(),
            }))
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completes_a_new_method_receiver_from_the_indexed_scope() {
        let source = "let mut numbers: List I32 = List.new ()\n";
        let path = std::env::temp_dir().join("staple-completion-new-method-receiver.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let index = index(&module, &typed);

        let items = index.named_method_items("numbers", source.len());
        assert!(
            items
                .iter()
                .any(|item| item.label == "push" && item.kind == Some(CompletionItemKind::METHOD))
        );
        assert!(!items.iter().any(|item| item.label == "new"));
    }

    #[test]
    fn dotted_module_path_covers_same_package_dependency_and_root() {
        assert_eq!(
            dotted_module_path(None, Path::new("/ws/src"), Path::new("/ws/src/tools/text.sta")),
            Some("tools.text".to_owned())
        );
        assert_eq!(
            dotted_module_path(Some("std"), Path::new("/std"), Path::new("/std/io.sta")),
            Some("std.io".to_owned())
        );
        assert_eq!(
            dotted_module_path(Some("std"), Path::new("/std"), Path::new("/std/core/list.sta")),
            Some("std.core.list".to_owned())
        );
        // A dependency `root.sta` collapses to the package name.
        assert_eq!(
            dotted_module_path(Some("dep"), Path::new("/dep"), Path::new("/dep/root.sta")),
            Some("dep".to_owned())
        );
        // The edited package's own `root.sta` is not importable by module path.
        assert_eq!(
            dotted_module_path(None, Path::new("/ws/src"), Path::new("/ws/src/root.sta")),
            None
        );
        // A file outside the source root is skipped.
        assert_eq!(
            dotted_module_path(None, Path::new("/ws/src"), Path::new("/other/x.sta")),
            None
        );
    }

    fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
        let mut spans = edits
            .iter()
            .map(|edit| {
                let start = crate::lsp::semantic::offset(text, edit.range.start).unwrap();
                let end = crate::lsp::semantic::offset(text, edit.range.end).unwrap();
                (start, end, edit.new_text.clone())
            })
            .collect::<Vec<_>>();
        spans.sort_by_key(|(start, ..)| std::cmp::Reverse(*start));
        let mut result = text.to_owned();
        for (start, end, new_text) in spans {
            result.replace_range(start..end, &new_text);
        }
        result
    }

    #[test]
    fn resolve_import_edits_inserts_a_new_use_line() {
        let text = "pub mod\n\ndef main = () => 0\n";
        let edits = resolve_import_edits(text, "std.io", "println").unwrap();
        assert_eq!(
            apply_edits(text, &edits),
            "pub mod\nuse std.io.println\n\ndef main = () => 0\n"
        );

        let bare = "def main = () => 0\n";
        let edits = resolve_import_edits(bare, "std.io", "println").unwrap();
        assert_eq!(
            apply_edits(bare, &edits),
            "use std.io.println\ndef main = () => 0\n"
        );
    }

    #[test]
    fn resolve_import_edits_appends_after_the_last_use() {
        let text = "pub mod\nuse std.list.map\n\ndef main = () => 0\n";
        let edits = resolve_import_edits(text, "std.io", "println").unwrap();
        assert_eq!(
            apply_edits(text, &edits),
            "pub mod\nuse std.list.map\nuse std.io.println\n\ndef main = () => 0\n"
        );
    }

    #[test]
    fn resolve_import_edits_merges_into_an_existing_selected_use() {
        let text = "use std.io.(read)\n\ndef main = () => 0\n";
        let edits = resolve_import_edits(text, "std.io", "println").unwrap();
        assert_eq!(
            apply_edits(text, &edits),
            "use std.io.(read, println)\n\ndef main = () => 0\n"
        );

        let public = "pub use std.io.(read)\n";
        let edits = resolve_import_edits(public, "std.io", "println").unwrap();
        assert_eq!(apply_edits(public, &edits), "pub use std.io.(read, println)\n");
    }

    #[test]
    fn resolve_import_edits_merges_a_single_item_dotted_use() {
        let text = "use std.io.read\n";
        let edits = resolve_import_edits(text, "std.io", "println").unwrap();
        assert_eq!(apply_edits(text, &edits), "use std.io.(read, println)\n");

        let public = "pub use std.io.read\n";
        let edits = resolve_import_edits(public, "std.io", "println").unwrap();
        assert_eq!(apply_edits(public, &edits), "pub use std.io.(read, println)\n");
    }

    #[test]
    fn resolve_import_edits_is_empty_when_already_imported() {
        assert_eq!(
            resolve_import_edits("use std.io.(println)\n", "std.io", "println"),
            Some(Vec::new())
        );
        assert_eq!(
            resolve_import_edits("use std.io.*\n", "std.io", "println"),
            Some(Vec::new())
        );
        assert_eq!(
            resolve_import_edits("use std.io.println\n", "std.io", "println"),
            Some(Vec::new())
        );
    }

    #[test]
    fn suggests_public_items_from_an_unimported_sibling_module() {
        let root = std::env::temp_dir().join(format!(
            "staple-completion-external-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("staple.kdl"), "package \"demo\" {\n    root \"src\"\n}\n").unwrap();
        std::fs::write(root.join("src/root.sta"), "pub mod\n").unwrap();
        std::fs::write(
            root.join("src/helpers.sta"),
            "pub mod\n\n/// Greets.\npub def greet = () => 0\npub type Widget = ()\nlet secret = 1\n",
        )
        .unwrap();
        let source = "pub mod\n\ndef main = () => 0\n";
        std::fs::write(root.join("src/main.sta"), source).unwrap();
        let path = std::fs::canonicalize(root.join("src/main.sta")).unwrap();
        let graph = staple_project::load_package_graph(&root.join("staple.kdl")).unwrap();
        let program = ProgramLoader::new()
            .with_package_graph(graph)
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib"))
            .load_package_graph_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let index = index(&module, &typed);
        std::fs::remove_dir_all(&root).unwrap();

        let items = index.items(source.find("0").unwrap());
        let greet = items
            .iter()
            .find(|item| item.label == "greet")
            .expect("greet should be offered from the sibling module");
        assert_eq!(greet.kind, Some(CompletionItemKind::FUNCTION));
        let data = greet.data.as_ref().expect("external item carries data");
        assert_eq!(data.get("staple.import").and_then(|v| v.as_str()), Some("helpers"));
        assert_eq!(data.get("staple.item").and_then(|v| v.as_str()), Some("greet"));

        assert!(items.iter().any(|item| item.label == "Widget"));
        // Private items are never offered.
        assert!(!items.iter().any(|item| item.label == "secret"));
        // A name already in scope is not duplicated as a cross-module entry.
        assert!(
            !items
                .iter()
                .any(|item| item.label == "main" && item.data.is_some())
        );
    }
}
