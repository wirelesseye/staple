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
    collector.module(resolved.syntax());
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
            Item::EffectDeclaration(value) => {
                self.declaration(&value.syntax, &value.name);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for operation in &value.operations {
                    self.declaration(&operation.syntax, &operation.name);
                }
            }
            Item::Statement(value) => self.statement(value),
            Item::UseDeclaration(_) | Item::MacroDeclaration(_) | Item::TraitImplementation(_) => {}
        }
    }

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Binding(value) => self.binding(value),
            Statement::PatternBinding(value) => self.pattern(&value.pattern),
            Statement::Expression(value) => self.expression(value),
            Statement::Assignment(value) => {
                self.expression(&value.target);
                self.expression(&value.value);
            }
            Statement::Return(value) => self.expression(&value.value),
            Statement::Break(value) => {
                if let Some(value) = &value.value {
                    self.expression(value);
                }
            }
            Statement::Continue(_) => {}
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
                for statement in &value.body.statements {
                    self.statement(statement);
                }
            }
            Expression::Handler(value) => {
                for clause in &value.clauses {
                    self.pattern(&clause.pattern);
                    self.expression(&clause.body);
                }
            }
            Expression::Handle(value) => {
                self.expression(&value.body);
                if let HandleKind::Manual(clauses) = &value.handler {
                    for clause in clauses {
                        self.pattern(&clause.pattern);
                        self.expression(&clause.body);
                    }
                }
            }
            Expression::Resume(value) => self.expression(&value.value),
            Expression::Block(value) => {
                for statement in &value.statements {
                    self.statement(statement);
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
            Expression::Infix(value) => {
                for operand in &value.operands {
                    self.expression(operand);
                }
            }
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
            Expression::Quote(value) => match &value.template {
                QuoteTemplate::Expression(expression) => self.expression(expression),
                QuoteTemplate::Item(item) => self.item(item),
                QuoteTemplate::Items(items) => items.iter().for_each(|item| self.item(item)),
                QuoteTemplate::Raw => {}
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
                self.named_type(&value.trait_name);
                for argument in &value.arguments {
                    self.ty(argument);
                }
                for member in &value.members {
                    self.add_resolved(&member.syntax, &member.name, false);
                    self.expression(&member.value);
                }
            }
            Item::EffectDeclaration(value) => {
                self.add_resolved(&value.syntax, &value.name, false);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
                for operation in &value.operations {
                    self.add_resolved(&operation.syntax, &operation.name, false);
                    self.ty(&operation.annotation);
                }
            }
            Item::Statement(value) => self.statement(value),
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
        match &value.kind {
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
            UseKind::Namespace | UseKind::Glob => {}
        }
    }

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Binding(value) => self.binding(value),
            Statement::PatternBinding(value) => {
                self.pattern(&value.pattern);
                self.expression(&value.value);
            }
            Statement::Assignment(value) => {
                self.expression(&value.target);
                self.expression(&value.value);
            }
            Statement::Return(value) => self.expression(&value.value),
            Statement::Break(value) => {
                if let Some(value) = &value.value {
                    self.expression(value);
                }
            }
            Statement::Continue(_) => {}
            Statement::Expression(value) => self.expression(value),
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
                for statement in &value.body.statements {
                    self.statement(statement);
                }
            }
            Expression::Handler(value) => {
                self.ty(&value.effect);
                for clause in &value.clauses {
                    self.add_resolved(&clause.syntax, &clause.operation, true);
                    self.pattern(&clause.pattern);
                    self.expression(&clause.body);
                }
            }
            Expression::Handle(value) => {
                self.expression(&value.body);
                match &value.handler {
                    HandleKind::Manual(clauses) => {
                        for clause in clauses {
                            self.add_resolved(&clause.syntax, &clause.operation, true);
                            self.pattern(&clause.pattern);
                            self.expression(&clause.body);
                        }
                    }
                    HandleKind::Value(handler) => self.expression(handler),
                }
            }
            Expression::Resume(value) => self.expression(&value.value),
            Expression::Block(value) => {
                for statement in &value.statements {
                    self.statement(statement);
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
            Expression::Infix(value) => {
                for operand in &value.operands {
                    self.expression(operand);
                }
                for operator in &value.operators {
                    self.add_resolved(&operator.syntax, &operator.name, true);
                }
            }
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
            Expression::Quote(value) => match &value.template {
                QuoteTemplate::Expression(expression) => self.expression(expression),
                QuoteTemplate::Item(item) => self.item(item),
                QuoteTemplate::Items(items) => items.iter().for_each(|item| self.item(item)),
                QuoteTemplate::Raw => {}
            },
            Expression::Name(value) => self.add_resolved(&value.syntax, &value.name, true),
            Expression::Splice(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        match pattern {
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
            Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
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
                for effect in &value.effects.effects {
                    self.ty(effect);
                }
                self.ty(&value.result);
            }
            Type::Handler(value) => {
                self.ty(&value.effect);
                for effect in &value.effects.effects {
                    self.ty(effect);
                }
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
            "type Wrapper = T => (value: T)\n",
            "trait Identity = T => { identity: T -> T }\n",
            "impl Identity I32 { def identity = value => value }\n",
            "def wrap: T => T -> Wrapper T = value => Wrapper (value: value)\n",
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
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "+"
                    && entry
                        .targets
                        .iter()
                        .any(|target| target.path.ends_with("std/core/number.sta"))
            }),
            "missing operator definition: {entries:?}"
        );
    }

    #[test]
    fn indexes_inline_module_names_and_members() {
        let source = concat!(
            "mod inner { pub def answer = () => 42 }\n",
            "use inner answer as selected\n",
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
            "use geometry (Point, origin)\n",
            "use geometry origin as make_origin\n",
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
