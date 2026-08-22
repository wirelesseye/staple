use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use stapler::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionTarget {
    pub path: PathBuf,
    pub range: Range<usize>,
    pub selection_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionEntry {
    pub range: Range<usize>,
    pub targets: Vec<DefinitionTarget>,
}

pub fn entries(
    module: &Module,
    resolved: &ResolvedModule,
    typed: Option<&TypedModule>,
) -> Vec<DefinitionEntry> {
    let mut targets = declaration_targets(resolved);
    let entry_path = &resolved.program().module(resolved.program().entry()).path;
    DeclarationCollector {
        resolved,
        path: entry_path,
        targets: &mut targets,
    }
    .module(module);
    let mut collector = Collector {
        resolved,
        typed,
        targets,
        entries: Vec::new(),
    };
    collector.module(module);
    collector.entries.sort_by(|left, right| {
        left.range
            .start
            .cmp(&right.range.start)
            .then(left.range.end.cmp(&right.range.end))
    });
    let mut merged: Vec<DefinitionEntry> = Vec::new();
    for entry in collector.entries {
        if let Some(previous) = merged.last_mut()
            && previous.range == entry.range
        {
            previous.targets.extend(entry.targets);
            previous.targets.sort_by(|left, right| {
                left.path
                    .cmp(&right.path)
                    .then(left.selection_range.start.cmp(&right.selection_range.start))
            });
            previous.targets.dedup();
        } else {
            merged.push(entry);
        }
    }
    merged
}

fn declaration_targets(resolved: &ResolvedModule) -> HashMap<DefinitionId, DefinitionTarget> {
    let mut targets = HashMap::new();
    for source_module in resolved.program().modules() {
        let mut collector = DeclarationCollector {
            resolved,
            path: &source_module.path,
            targets: &mut targets,
        };
        collector.module(&source_module.syntax);
    }
    for source_module in resolved.program().modules() {
        let definition = DefinitionId::Module(source_module.id);
        targets
            .entry(definition)
            .or_insert_with(|| DefinitionTarget {
                path: source_module.path.clone(),
                range: 0..0,
                selection_range: 0..0,
            });
    }
    targets
}

struct DeclarationCollector<'a> {
    resolved: &'a ResolvedModule,
    path: &'a PathBuf,
    targets: &'a mut HashMap<DefinitionId, DefinitionTarget>,
}

impl DeclarationCollector<'_> {
    fn module(&mut self, module: &Module) {
        for item in &module.items {
            self.item(item);
        }
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Modified(value) => self.item(&value.item),
            Item::VisibilityMacroInvocation(value) => self.expression(&value.expression),
            Item::VisibilitySplice(value) => self.item(&value.item),
            Item::RepeatedItemSplice(_) => {}
            Item::Submodule(value) => {
                if let Some(id) = self.resolved.program().child_module(value.syntax.id) {
                    self.insert(DefinitionId::Module(id), &value.syntax, &value.name);
                }
                self.module(&value.module);
            }
            Item::ExternBlock(value) => {
                for binding in &value.bindings {
                    self.binding(binding);
                }
            }
            Item::TypeDeclaration(value) => {
                self.declaration(&value.syntax, &value.name);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
            }
            Item::TraitDeclaration(value) => {
                self.declaration(&value.syntax, &value.name);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for member in &value.members {
                    self.declaration(&member.syntax, &member.name);
                }
            }
            Item::MacroDeclaration(value) => {
                self.declaration(&value.syntax, &value.name);
                if let Some(expression) = &value.value {
                    self.expression(expression);
                }
            }
            value @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.statement(value),
            Item::TraitImplementation(value) => {
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
            }
            Item::UseDeclaration(_) => {}
        }
    }

    fn statement(&mut self, statement: &Item) {
        match statement {
            Item::Binding(value) => self.binding(value),
            Item::PatternBinding(value) => self.pattern(&value.pattern),
            Item::Expression(value) => self.expression(value),
            Item::Assignment(value) => {
                self.expression(&value.target);
                self.expression(&value.value);
            }
            Item::Return(value) => self.expression(&value.value),
            Item::Break(value) => {
                if let Some(value) = &value.value {
                    self.expression(value);
                }
            }
            Item::Continue(_) => {}
            Item::Submodule(value) => {
                if let Some(id) = self.resolved.program().child_module(value.syntax.id) {
                    self.insert(DefinitionId::Module(id), &value.syntax, &value.name);
                }
                self.module(&value.module);
            }
            Item::TypeDeclaration(value) => {
                self.declaration(&value.syntax, &value.name);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
            }
            Item::UseDeclaration(_) => {}
            _ => {}
        }
    }

    fn binding(&mut self, binding: &Binding) {
        self.declaration(&binding.syntax, &binding.name);
        for parameter in &binding.type_parameters {
            self.type_parameter(parameter);
        }
        if let Some(value) = &binding.value {
            self.expression(value);
        }
    }

    fn expression(&mut self, expression: &Expression) {
        match expression {
            Expression::Function(value) => {
                self.pattern(&value.pattern);
                self.expression(&value.body);
            }
            Expression::Satisfies(value) => self.expression(&value.value),
            Expression::Match(value) => {
                self.expression(&value.subject);
                for arm in &value.arms {
                    self.pattern(&arm.pattern);
                    self.expression(&arm.body);
                }
            }
            Expression::Loop(value) => {
                for item in &value.body.items {
                    self.item(item);
                }
            }
            Expression::Resource(_) => {}
            Expression::With(value) => {
                self.expression(&value.value);
                for item in &value.body.items {
                    self.item(item);
                }
            }
            Expression::Block(value) => {
                for item in &value.items {
                    self.item(item);
                }
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    self.expression(&element.value);
                }
            }
            Expression::Call(value) => {
                self.expression(&value.callee);
                self.expression(&value.argument);
            }
            Expression::Access(value) => self.expression(&value.value),
            Expression::Index(value) => {
                self.expression(&value.value);
                self.expression(&value.index);
            }
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
            Expression::Quote(value) => {
                match &value.template {
                    QuoteTemplate::Expression(expression) => self.expression(expression),
                    QuoteTemplate::Item(item) => self.item(item),
                    QuoteTemplate::Items(items) => items.iter().for_each(|item| self.item(item)),
                    QuoteTemplate::Raw => {}
                }
            },
            Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::At(at) => {
                self.pattern(&Pattern::Binding(at.binding.as_ref().clone()));
                self.pattern(&at.pattern);
            }
            Pattern::Binding(value) => self.declaration(&value.syntax, &value.name),
            Pattern::Product(value) => {
                for element in &value.elements {
                    self.pattern(element);
                }
            }
            Pattern::Nominal(value) => self.pattern(&value.argument),
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }

    fn type_parameter(&mut self, parameter: &TypeParameterPattern) {
        match parameter {
            TypeParameterPattern::Binding(value) => self.declaration(&value.syntax, &value.name),
            TypeParameterPattern::Product(value) => {
                for element in &value.elements {
                    self.type_parameter(element);
                }
            }
            TypeParameterPattern::Splice(_) => {}
        }
    }

    fn declaration(&mut self, syntax: &Syntax, name: &str) {
        for definition in self.resolved.definitions_for(syntax.id) {
            if self.resolved.declaration_syntax(definition) == Some(syntax.id) {
                self.insert(definition, syntax, name);
            }
        }
    }

    fn insert(&mut self, definition: DefinitionId, syntax: &Syntax, name: &str) {
        if let Some(Span::User { source, range, .. }) = syntax.identifier_origin(name, false) {
            let range = range.clone();
            self.targets.insert(
                definition,
                DefinitionTarget {
                    path: source
                        .as_deref()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| self.path.clone()),
                    range: range.clone(),
                    selection_range: range,
                },
            );
            return;
        }
        let selection_range =
            token_range(syntax, name, false).unwrap_or_else(|| syntax_range(syntax));
        self.targets.insert(
            definition,
            DefinitionTarget {
                path: self.path.clone(),
                range: syntax_range(syntax),
                selection_range,
            },
        );
    }
}

struct Collector<'a> {
    resolved: &'a ResolvedModule,
    typed: Option<&'a TypedModule>,
    targets: HashMap<DefinitionId, DefinitionTarget>,
    entries: Vec<DefinitionEntry>,
}

impl Collector<'_> {
    fn module(&mut self, module: &Module) {
        for item in &module.items {
            self.item(item);
        }
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Modified(value) => {
                for modifier in &value.modifiers {
                    if let Some(definitions) = self.macro_invocation_definitions(modifier.syntax.id)
                    {
                        self.add(&modifier.syntax, &modifier.name, &definitions, true);
                        if let Some(namespace) = &modifier.namespace
                            && let Some(module) = definitions
                                .iter()
                                .find_map(|definition| self.resolved.definition_module(*definition))
                        {
                            self.add(
                                &modifier.syntax,
                                namespace,
                                &[DefinitionId::Module(module)],
                                false,
                            );
                        }
                    }
                    if let Some(expression) = modifier
                        .argument
                        .as_ref()
                        .and_then(|argument| argument.expression.as_ref())
                    {
                        self.expression(expression);
                    }
                }
                self.item(&value.item);
            }
            Item::VisibilityMacroInvocation(value) => self.expression(&value.expression),
            Item::VisibilitySplice(value) => self.item(&value.item),
            Item::RepeatedItemSplice(_) => {}
            Item::UseDeclaration(value) => self.use_declaration(value),
            Item::Submodule(value) => {
                if let Some(id) = self.resolved.program().child_module(value.syntax.id) {
                    self.add(
                        &value.syntax,
                        &value.name,
                        &[DefinitionId::Module(id)],
                        false,
                    );
                }
                self.module(&value.module);
            }
            Item::ExternBlock(value) => {
                for binding in &value.bindings {
                    self.binding(binding);
                }
            }
            Item::TypeDeclaration(value) => {
                self.add_resolved(&value.syntax, &value.name, false);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for bound in &value.trait_bounds {
                    self.trait_bound(bound);
                }
                if let Some(underlying) = &value.underlying {
                    self.ty(underlying);
                }
            }
            Item::MacroDeclaration(value) => {
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for bound in &value.trait_bounds {
                    self.trait_bound(bound);
                }
                if let Some(annotation) = &value.annotation {
                    self.ty(annotation);
                }
                if let Some(expression) = &value.value {
                    self.expression(expression);
                }
            }
            Item::TraitDeclaration(value) => {
                self.add_resolved(&value.syntax, &value.name, false);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for dependency in &value.functional_dependencies {
                    for determinant in &dependency.determinants {
                        self.named_type(determinant);
                    }
                    self.named_type(&dependency.dependent);
                }
                for bound in &value.prerequisites {
                    self.trait_bound(bound);
                }
                for member in &value.members {
                    self.add_resolved(&member.syntax, &member.name, false);
                    self.ty(&member.annotation);
                    if let Some(default) = &member.default {
                        self.expression(default);
                    }
                }
            }
            Item::TraitImplementation(value) => {
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for bound in &value.trait_bounds {
                    self.trait_bound(bound);
                }
                self.named_type(&value.trait_name);
                for argument in &value.arguments {
                    self.ty(argument);
                }
                for member in &value.members {
                    self.add_resolved(&member.syntax, &member.name, false);
                    self.expression(&member.value);
                }
            }
            value @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.statement(value),
        }
    }

    fn use_declaration(&mut self, value: &UseDeclaration) {
        if let Some(name) = value.path.last() {
            let definitions = self
                .resolved
                .import_definitions(value.syntax.id, name)
                .to_vec();
            self.add(&value.syntax, name, &definitions, false);
        }
        match self.resolved.program().use_kind(value) {
            UseKind::Selected(names) => {
                for name in names {
                    let definitions = self
                        .resolved
                        .import_definitions(value.syntax.id, name)
                        .to_vec();
                    self.add(&value.syntax, name, &definitions, true);
                }
            }
            UseKind::Renamed { item, alias } => {
                for name in [item, alias] {
                    let definitions = self
                        .resolved
                        .import_definitions(value.syntax.id, name)
                        .to_vec();
                    self.add(&value.syntax, name, &definitions, name == alias);
                }
            }
            UseKind::Dotted | UseKind::Namespace | UseKind::Glob => {}
        }
    }

    fn statement(&mut self, statement: &Item) {
        match statement {
            Item::Binding(value) => self.binding(value),
            Item::PatternBinding(value) => {
                self.pattern(&value.pattern);
                self.expression(&value.value);
            }
            Item::Assignment(value) => {
                self.expression(&value.target);
                self.expression(&value.value);
            }
            Item::Return(value) => self.expression(&value.value),
            Item::Break(value) => {
                if let Some(value) = &value.value {
                    self.expression(value);
                }
            }
            Item::Continue(_) => {}
            Item::Expression(value) => self.expression(value),
            Item::Submodule(value) => {
                if let Some(id) = self.resolved.program().child_module(value.syntax.id) {
                    self.add(
                        &value.syntax,
                        &value.name,
                        &[DefinitionId::Module(id)],
                        false,
                    );
                }
                self.module(&value.module);
            }
            Item::TypeDeclaration(value) => {
                self.add_resolved(&value.syntax, &value.name, false);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for bound in &value.trait_bounds {
                    self.trait_bound(bound);
                }
                if let Some(underlying) = &value.underlying {
                    self.ty(underlying);
                }
            }
            Item::UseDeclaration(value) => self.use_declaration(value),
            _ => {}
        }
    }

    fn binding(&mut self, binding: &Binding) {
        self.add_resolved(&binding.syntax, &binding.name, false);
        for parameter in &binding.type_parameters {
            self.type_parameter(parameter);
        }
        for bound in &binding.trait_bounds {
            self.trait_bound(bound);
        }
        if let Some(annotation) = &binding.annotation {
            self.ty(annotation);
        }
        if let Some(value) = &binding.value {
            self.expression(value);
        }
    }

    fn expression(&mut self, expression: &Expression) {
        if let Some(definitions) = self.macro_invocation_definitions(expression.syntax().id) {
            match expression {
                Expression::Name(value) => {
                    self.add(&value.syntax, &value.name, &definitions, true);
                    return;
                }
                Expression::Access(value) => {
                    if let Accessor::Name(name) = &value.accessor {
                        self.add(&value.syntax, name, &definitions, true);
                    }
                    if let Expression::Name(namespace) = value.value.as_ref()
                        && let Some(module) = definitions
                            .iter()
                            .find_map(|definition| self.resolved.definition_module(*definition))
                    {
                        self.add(
                            &namespace.syntax,
                            &namespace.name,
                            &[DefinitionId::Module(module)],
                            false,
                        );
                    }
                    return;
                }
                _ => {}
            }
        }
        match expression {
            Expression::Function(value) => {
                self.pattern(&value.pattern);
                self.expression(&value.body);
            }
            Expression::Satisfies(value) => {
                self.expression(&value.value);
                self.ty(&value.ty);
            }
            Expression::Match(value) => {
                self.expression(&value.subject);
                for arm in &value.arms {
                    self.pattern(&arm.pattern);
                    self.expression(&arm.body);
                }
            }
            Expression::Loop(value) => {
                for item in &value.body.items {
                    self.item(item);
                }
            }
            Expression::Resource(value) => self.ty(&value.resource),
            Expression::With(value) => {
                self.ty(&value.resource);
                self.expression(&value.value);
                for item in &value.body.items {
                    self.item(item);
                }
            }
            Expression::Block(value) => {
                for item in &value.items {
                    self.item(item);
                }
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    self.expression(&element.value);
                }
            }
            Expression::Call(value) => {
                self.expression(&value.callee);
                self.expression(&value.argument);
            }
            Expression::Access(value) => {
                if let Accessor::Name(name) = &value.accessor {
                    let definitions = self.definitions_for(value.syntax.id);
                    self.add(&value.syntax, name, &definitions, true);
                    if let Expression::Name(namespace) = value.value.as_ref()
                        && let Some(module) = definitions
                            .iter()
                            .find_map(|definition| self.resolved.definition_module(*definition))
                    {
                        self.add(
                            &namespace.syntax,
                            &namespace.name,
                            &[DefinitionId::Module(module)],
                            false,
                        );
                    }
                } else {
                    self.expression(&value.value);
                }
            }
            Expression::Index(value) => {
                self.expression(&value.value);
                self.expression(&value.index);
            }
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
            Expression::Quote(value) => {
                self.add_resolved(&value.syntax, value.kind.name(), false);
                match &value.template {
                    QuoteTemplate::Expression(expression) => self.expression(expression),
                    QuoteTemplate::Item(item) => self.item(item),
                    QuoteTemplate::Items(items) => items.iter().for_each(|item| self.item(item)),
                    QuoteTemplate::Raw => {}
                }
            },
            Expression::Name(value) => self.add_resolved(&value.syntax, &value.name, true),
            Expression::Splice(value) => self.add_resolved(&value.syntax, &value.name, true),
            Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::At(at) => {
                self.pattern(&Pattern::Binding(at.binding.as_ref().clone()));
                self.pattern(&at.pattern);
            }
            Pattern::Binding(value) => {
                self.add_resolved(&value.syntax, &value.name, true);
                self.ty(&value.ty);
            }
            Pattern::Product(value) => {
                for element in &value.elements {
                    self.pattern(element);
                }
            }
            Pattern::Nominal(value) => {
                self.add_resolved(&value.syntax, &value.name, false);
                self.pattern(&value.argument);
            }
            Pattern::Wildcard(value) => self.ty(&value.ty),
            Pattern::Splice(value) => self.add_resolved(&value.syntax, &value.name, true),
            Pattern::StringLiteral(_) => {}
        }
    }

    fn type_parameter(&mut self, parameter: &TypeParameterPattern) {
        match parameter {
            TypeParameterPattern::Binding(value) => {
                self.add_resolved(&value.syntax, &value.name, false)
            }
            TypeParameterPattern::Product(value) => {
                for element in &value.elements {
                    self.type_parameter(element);
                }
            }
            TypeParameterPattern::Splice(_) => {}
        }
    }

    fn trait_bound(&mut self, bound: &TraitBound) {
        self.named_type(&bound.trait_name);
        for argument in &bound.arguments {
            self.ty(argument);
        }
    }

    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Named(value) => self.named_type(value),
            Type::Product(value) => {
                for element in &value.elements {
                    self.ty(&element.ty);
                }
            }
            Type::Sum(value) => {
                for alternative in &value.alternatives {
                    self.ty(alternative);
                }
            }
            Type::Function(value) => {
                self.ty(&value.parameter);
                for resource in &value.resources.resources {
                    self.ty(resource);
                }
                self.ty(&value.result);
            }
            Type::Application(value) => {
                self.ty(&value.callee);
                self.ty(&value.argument);
            }
            Type::Repeated(value) => self.ty(&value.element),
            Type::Inferred(_) | Type::StringLiteral(_) | Type::Splice(_) => {}
        }
    }

    fn named_type(&mut self, value: &NamedType) {
        let definitions = self.definitions_for(value.syntax.id);
        self.add(&value.syntax, &value.name, &definitions, true);
        if let Some(namespace) = &value.namespace
            && let Some(module) = definitions
                .iter()
                .find_map(|definition| self.resolved.definition_module(*definition))
        {
            self.add(
                &value.syntax,
                namespace,
                &[DefinitionId::Module(module)],
                false,
            );
        }
    }

    fn add_resolved(&mut self, syntax: &Syntax, name: &str, last: bool) {
        let definitions = self.definitions_for(syntax.id);
        self.add(syntax, name, &definitions, last);
    }

    fn definitions_for(&self, syntax: SyntaxId) -> Vec<DefinitionId> {
        let mut definitions = self.resolved.definitions_for(syntax);
        if let Some(dispatch) = self
            .typed
            .and_then(|typed| typed.trait_dispatch_for(syntax))
        {
            definitions.retain(|definition| !matches!(definition, DefinitionId::TraitMethod(_)));
            definitions.push(DefinitionId::TraitMethod(dispatch.method));
        }
        definitions
    }

    fn macro_invocation_definitions(&self, syntax: SyntaxId) -> Option<Vec<DefinitionId>> {
        let declaration = self.resolved.macro_invocation_for(syntax)?.declaration;
        Some(self.resolved.definitions_for(declaration))
    }

    fn add(&mut self, syntax: &Syntax, name: &str, definitions: &[DefinitionId], last: bool) {
        let Some(range) = token_range(syntax, name, last) else {
            return;
        };
        let mut targets = definitions
            .iter()
            .filter_map(|definition| self.targets.get(definition).cloned())
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.selection_range.start.cmp(&right.selection_range.start))
        });
        targets.dedup();
        if !targets.is_empty() {
            self.entries.push(DefinitionEntry { range, targets });
        }
    }
}

fn token_range(syntax: &Syntax, name: &str, last: bool) -> Option<Range<usize>> {
    let tokens = syntax.tokens();
    if last {
        tokens
            .iter()
            .rev()
            .find(|token| token.text == name)
            .map(|token| token.span.clone())
    } else {
        tokens
            .iter()
            .find(|token| token.text == name)
            .map(|token| token.span.clone())
    }
}

fn syntax_range(syntax: &Syntax) -> Range<usize> {
    let first = syntax.tokens().iter().find(|token| !token.kind.is_trivia());
    let last = syntax
        .tokens()
        .iter()
        .rev()
        .find(|token| !token.kind.is_trivia());
    match (first, last) {
        (Some(first), Some(last)) => first.span.start..last.span.end,
        _ => syntax.span.to_range(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_local_values_types_and_trait_members() {
        let source = concat!(
            "type Wrapper T = (value: T)\n",
            "trait Identity T { identity: T -> T }\n",
            "impl Identity I32 { def identity = value => value }\n",
            "def wrap: <T> T -> Wrapper T = value => Wrapper (value: value)\n",
            "def apply = () => identity 1\n",
            "let (first, second) = (1, 2)\n",
            "first\n",
            "let total = 1 + 2\n",
            "apply\n",
        );
        let path = std::env::temp_dir().join("staple-definition-local-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        assert_target(source, &entries, "apply", "apply");
        assert_target(source, &entries, "Wrapper", "Wrapper");
        assert_target(source, &entries, "identity", "identity");
        assert_target(source, &entries, "value", "value");
        assert_target(source, &entries, "first", "first");
    }

    #[test]
    fn indexes_generic_trait_implementation_type_parameters() {
        let source = concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl Bound I32 { def check = value => True }\n",
            "impl <T where Bound T> Target T { def act = value => value }\n",
        );
        let path = std::env::temp_dir().join("staple-definition-generic-impl-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        assert_target(source, &entries, "T", "T");
        assert_target(source, &entries, "Bound", "Bound");
    }

    #[test]
    fn indexes_inline_module_names_and_members() {
        let source = concat!(
            "mod inner { pub def answer = () => 42 }\n",
            "use inner.answer as selected\n",
            "selected ()\n",
            "inner.answer ()\n",
        );
        let path = std::env::temp_dir().join("staple-definition-inline-module-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &resolved, None);

        assert_target(source, &entries, "selected", "answer");
        assert_target(source, &entries, "answer", "answer");
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "inner"
                && entry
                    .targets
                    .iter()
                    .any(|target| &source[target.selection_range.clone()] == "inner")
        }));
    }

    #[test]
    fn indexes_function_and_modifier_macro_invocations() {
        let source = concat!(
            "use std.syntax.parse_quote\n",
            "macro identity: Expr -> Expr = value => parse_quote { $value }\n",
            "macro @keep: Item -> Item = item => item\n",
            "let answer = identity 42\n",
            "@keep\n",
            "let kept = answer\n",
        );
        let path = std::env::temp_dir().join("staple-definition-macro-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &resolved, None);

        assert_target(source, &entries, "identity", "identity");
        assert_target(source, &entries, "keep", "keep");
    }

    #[test]
    fn indexes_import_clauses_aliases_and_imported_references() {
        let root = std::env::temp_dir().join(format!(
            "staple-definition-import-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let dependency = root.join("geometry.sta");
        std::fs::write(
            &dependency,
            concat!(
                "pub(repr) type Point = (x: I32, y: I32)\n",
                "pub def origin = () => Point (x: 0, y: 0)\n",
            ),
        )
        .unwrap();
        let dependency = std::fs::canonicalize(dependency).unwrap();
        let source = concat!(
            "use geometry\n",
            "use geometry.(Point, origin)\n",
            "use geometry.origin as make_origin\n",
            "let start: Point = make_origin ()\n",
            "let other = geometry.origin ()\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &resolved, None);

        for name in ["Point", "origin", "make_origin"] {
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == name
                        && entry.targets.iter().any(|target| target.path == dependency)
                }),
                "missing imported target for {name}: {entries:?}"
            );
        }
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "geometry"
                && entry.targets.iter().any(|target| target.path == dependency)
        }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn does_not_project_macro_definition_ranges_onto_call_site_tokens() {
        let source = concat!(
            "use std.io.println\n",
            "\n",
            "typegroup A = {\n",
            "    Hello\n",
            "}\n",
            "\n",
            "macro what = x => parse_quote {$x}\n",
            "\n",
            "def main = () => {\n",
            "    let generated = A.Hello\n",
            "    let x = 3\n",
            "    let y: I32 = x + 30\n",
            "    let condition: Bool = True\n",
            "    if {\n",
            "        condition => { println \"123\" },\n",
            "    }\n",
            "    println \"Hello, world!\"\n",
            "}\n",
        );
        let path = std::env::temp_dir().join("staple-definition-if-macro-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        let tokens = lex(source);
        for entry in &entries {
            assert!(
                tokens.iter().any(|token| token.span == entry.range),
                "definition range {:?} ({:?}) does not match a source token",
                entry.range,
                &source[entry.range.clone()]
            );
        }
        for token in tokens.iter().filter(|token| {
            matches!(
                token.kind,
                TokenKind::String | TokenKind::Integer | TokenKind::LBrace | TokenKind::RBrace
            )
        }) {
            assert!(
                entries.iter().all(|entry| {
                    entry.range.end <= token.span.start || token.span.end <= entry.range.start
                }),
                "literal or brace {:?} received a definition entry",
                token.text
            );
        }
        for (origin, target_path) in [("True", "boolean.sta"), ("println", "io.sta")] {
            assert!(entries.iter().any(|entry| {
                &source[entry.range.clone()] == origin
                    && entry
                        .targets
                        .iter()
                        .any(|target| target.path.ends_with(target_path))
            }));
        }

        let variant = source.find("Hello").unwrap();
        let reference = source.find("A.Hello").unwrap() + 2;
        assert!(
            entries.iter().any(|entry| {
                entry.range == (reference..reference + "Hello".len())
                    && entry.targets.iter().any(|target| {
                        target.path.ends_with(path.file_name().unwrap())
                            && target.range == (variant..variant + "Hello".len())
                            && target.selection_range == (variant..variant + "Hello".len())
                    })
            }),
            "missing generated variant origin: {entries:?}"
        );

        let boolean = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib/std/core/boolean.sta"),
        )
        .unwrap();
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "True"
                    && entry.targets.iter().any(|target| {
                        target.path.ends_with("boolean.sta")
                            && &boolean[target.selection_range.clone()] == "True"
                            && target.range == target.selection_range
                    })
            }),
            "missing Bool variant origin: {entries:?}"
        );
    }

    #[test]
    fn indexes_macro_and_helper_body_items_and_helper_annotations() {
        let stdlib = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let path = stdlib.join("std/core/flow.sta");
        let source = std::fs::read_to_string(&path).unwrap();
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib)
            .load_source_at(&path, &source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(&source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        assert_target(&source, &entries, "otherwise", "otherwise");
        assert_target(&source, &entries, "if_clauses", "if_clauses");
        assert_target(&source, &entries, "body", "body");
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "IntoIterator"
                && entry.targets.iter().any(|target| target.path.ends_with("iterator.sta"))
        }), "{entries:?}");
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "Expr"
                && entry.targets.iter().any(|target| target.path.ends_with("syntax.sta"))
        }), "{entries:?}");
    }

    #[test]
    fn indexes_quote_and_parse_quote_keywords() {
        let source = concat!(
            "use std.syntax.(quote, parse_quote, Expr, Syntax)\n",
            "macro raw: Expr -> Syntax = value: Expr => quote { $value }\n",
            "macro parsed: Expr -> Expr = value: Expr => parse_quote { $value }\n",
        );
        let path = std::env::temp_dir().join("staple-definition-quote-keywords.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        for keyword in ["quote", "parse_quote"] {
            assert!(entries.iter().any(|entry| {
                &source[entry.range.clone()] == keyword
                    && entry.targets.iter().any(|target| target.path.ends_with("syntax.sta"))
            }), "missing definition for {keyword}: {entries:?}");
        }
    }

    fn assert_target(source: &str, entries: &[DefinitionEntry], origin: &str, target: &str) {
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == origin
                    && entry
                        .targets
                        .iter()
                        .any(|target_entry| &source[target_entry.selection_range.clone()] == target)
            }),
            "missing {origin} -> {target}: {entries:?}"
        );
    }
}
