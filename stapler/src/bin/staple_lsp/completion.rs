use std::collections::HashMap;
use std::ops::Range;

use lsp_types::{CompletionItem, CompletionItemKind};
use stapler::*;

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
}

#[derive(Debug, Clone)]
struct MethodSite {
    receiver_end: usize,
    items: Vec<CompletionItem>,
}

pub fn index(module: &Module, typed: &TypedModule) -> CompletionIndex {
    let mut collector = Collector {
        typed,
        index: CompletionIndex::default(),
    };
    let entry = typed.resolved().program().entry();
    collector.module(module, entry);
    collector.index
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
        items.into_iter().map(|(_, item)| item).collect()
    }

    pub fn method_items(&self, receiver_end: usize) -> Vec<CompletionItem> {
        self.methods
            .iter()
            .filter(|site| site.receiver_end == receiver_end)
            .max_by_key(|site| site.items.len())
            .map(|site| site.items.clone())
            .unwrap_or_default()
    }
}

struct Collector<'a> {
    typed: &'a TypedModule,
    index: CompletionIndex,
}

impl Collector<'_> {
    fn module(&mut self, module: &Module, id: ModuleId) {
        let range = syntax_range(&module.syntax);
        let mut root = Scope {
            range: range.clone(),
            candidates: Vec::new(),
        };
        if let Some(definitions) = self.typed.resolved().visible_definitions(id) {
            for (name, definitions) in definitions {
                for definition in definitions {
                    if let Some(candidate) = self.definition(name, *definition, range.start) {
                        root.candidates.push(candidate);
                    }
                }
            }
        }
        self.sequential_items(&module.items, &mut root.candidates, range.start);
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

    fn item_declarations(
        &self,
        item: &Item,
        candidates: &mut Vec<Candidate>,
        scope_start: usize,
    ) {
        match item {
            Item::Modified(value) => {
                self.item_declarations(&value.item, candidates, scope_start)
            }
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
        let prefix = binding.keyword();
        if let Some(detail) = candidate.item.detail.take() {
            candidate.item.detail = Some(format!("{prefix} {}: {detail}", binding.name));
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
        Some(Candidate {
            available_from,
            namespace,
            item: CompletionItem {
                label: name.to_owned(),
                kind: Some(kind),
                detail,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn completes_keywords_module_names_imports_and_lexical_bindings() {
        let root =
            std::env::temp_dir().join(format!("staple-completion-scopes-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("dependency.sta"), "pub let imported = 1\n").unwrap();
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
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
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
    fn inner_bindings_shadow_outer_values() {
        let source = "let value = 1\ndef choose = value: I32 => value\n";
        let path = std::env::temp_dir().join("staple-completion-shadow.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
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
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
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
    fn maps_imported_definition_namespaces_to_completion_kinds() {
        let root =
            std::env::temp_dir().join(format!("staple-completion-kinds-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            concat!(
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
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
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
        let source = concat!("let mut numbers: List I32 = List.new ()\n", "numbers\n",);
        let path = std::env::temp_dir().join("staple-completion-methods.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
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
}
