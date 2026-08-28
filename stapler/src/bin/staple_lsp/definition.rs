use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
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

#[cfg(test)]
pub fn entries(
    module: &Module,
    resolved: &ResolvedModule,
    typed: Option<&TypedModule>,
) -> Vec<DefinitionEntry> {
    let path = &resolved.program().module(resolved.program().entry()).path;
    entries_at_path(path, module, resolved, typed)
}

pub fn entries_at_path(
    path: &Path,
    module: &Module,
    resolved: &ResolvedModule,
    typed: Option<&TypedModule>,
) -> Vec<DefinitionEntry> {
    // The editor hands us its own freshly parsed `module`, still carrying the
    // unexpanded macro call sites we want to offer go-to-definition for. Its
    // `SyntaxId`s are numbered from zero, which only lines up with the loaded
    // program when this file was parsed first (the standalone case). Under a
    // package graph the package-root module is parsed ahead of it, so every id
    // in the editor copy is shifted by a constant relative to the same node in
    // the program, and each `definitions_for`/`namespace_for` lookup during the
    // walk would otherwise land on an unrelated node and invent a bogus target.
    // Recover that constant from the program's own module for this path and
    // rebase the editor walk's lookups by it.
    let editor_id_offset = resolved
        .program()
        .modules()
        .iter()
        .find(|source| {
            !source.companion
                && source.path == path
                && source
                    .parent
                    .is_none_or(|parent| resolved.program().module(parent).path != source.path)
        })
        .map_or(0, |source| {
            source.syntax.syntax.id.0 as i64 - module.syntax.id.0 as i64
        });
    let mut targets = declaration_targets(resolved);
    let entry_path = &resolved.program().module(resolved.program().entry()).path;
    DeclarationCollector {
        resolved,
        path: entry_path,
        targets: &mut targets,
        id_offset: editor_id_offset,
    }
    .module(module);
    let mut collector = Collector {
        resolved,
        typed,
        targets,
        entries: Vec::new(),
        path,
        id_offset: editor_id_offset,
    };
    collector.module(module);
    collector.id_offset = 0;
    collector.module(resolved.syntax());
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
            id_offset: 0,
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
    /// Added to every `SyntaxId` taken from the walked tree before it is used
    /// to query the resolver, so an editor-owned copy whose ids start from
    /// zero still lines up with the loaded program. Zero for program-owned
    /// trees.
    id_offset: i64,
}

impl DeclarationCollector<'_> {
    fn resolved_id(&self, id: SyntaxId) -> SyntaxId {
        SyntaxId((id.0 as i64 + self.id_offset) as usize)
    }

    fn module(&mut self, module: &Module) {
        for item in &module.items {
            self.item(item);
        }
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Modified(value) => self.item(&value.item),
            Item::VisibilityMacroInvocation(value) => {
                for modifier in &value.modifiers {
                    if let Some(expression) = modifier
                        .argument
                        .as_ref()
                        .and_then(|argument| argument.expression.as_ref())
                    {
                        self.expression(expression);
                    }
                }
                self.expression(&value.expression)
            }
            Item::VisibilitySplice(value) => self.item(&value.item),
            Item::RepeatedItemSplice(_) => {}
            Item::Submodule(value) => {
                if let Some(id) = self
                    .resolved
                    .program()
                    .child_module(self.resolved_id(value.syntax.id))
                {
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
            | Item::Expression(_)) => self.block_item(value),
            Item::TraitImplementation(value) => {
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter);
                }
            }
            Item::UseDeclaration(_) => {}
        }
    }

    fn block_item(&mut self, item: &Item) {
        match item {
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
                if let Some(id) = self
                    .resolved
                    .program()
                    .child_module(self.resolved_id(value.syntax.id))
                {
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
            Expression::StringTemplate(value) => {
                for part in &value.parts {
                    if let StringTemplatePart::Interpolation(value) = part {
                        self.expression(&value.expression);
                    }
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
            Expression::Logical(value) => {
                self.expression(&value.left);
                self.expression(&value.right);
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
            TypeParameterPattern::Effect(value) => self.declaration(&value.syntax, &value.name),
            TypeParameterPattern::Product(value) => {
                for element in &value.elements {
                    self.type_parameter(element);
                }
            }
            TypeParameterPattern::Splice(_) => {}
        }
    }

    fn declaration(&mut self, syntax: &Syntax, name: &str) {
        let id = self.resolved_id(syntax.id);
        for definition in self.resolved.definitions_for(id) {
            if self.resolved.declaration_syntax(definition) == Some(id) {
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
    path: &'a Path,
    /// Added to every `SyntaxId` taken from the walked tree before it is used
    /// to query the resolver or type checker. Non-zero while walking an
    /// editor-owned surface tree whose ids start from zero but whose backing
    /// module sits further along the program's shared id sequence; zero for
    /// program-owned trees.
    id_offset: i64,
}

impl Collector<'_> {
    fn resolved_id(&self, id: SyntaxId) -> SyntaxId {
        SyntaxId((id.0 as i64 + self.id_offset) as usize)
    }

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
            Item::VisibilityMacroInvocation(value) => {
                for modifier in &value.modifiers {
                    if let Some(expression) = modifier
                        .argument
                        .as_ref()
                        .and_then(|argument| argument.expression.as_ref())
                    {
                        self.expression(expression);
                    }
                }
                self.expression(&value.expression)
            }
            Item::VisibilitySplice(value) => self.item(&value.item),
            Item::RepeatedItemSplice(_) => {}
            Item::UseDeclaration(value) => self.use_declaration(value),
            Item::Submodule(value) => {
                if let Some(id) = self
                    .resolved
                    .program()
                    .child_module(self.resolved_id(value.syntax.id))
                {
                    // A `companion` header points at the type it extends, not
                    // at the companion submodule.
                    let definitions = self.namespace_definitions(id);
                    self.add(&value.syntax, &value.name, &definitions, false);
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
            | Item::Expression(_)) => self.block_item(value),
        }
    }

    fn use_declaration(&mut self, value: &UseDeclaration) {
        let resolved_kind = self.resolved.program().use_kind(value).clone();
        self.use_module_segments(value, &resolved_kind);
        self.inline_use_path_segments(value, &resolved_kind);
        if let Some(name) = value.path.last() {
            let definitions = self
                .resolved
                .import_definitions(self.resolved_id(value.syntax.id), name)
                .to_vec();
            self.add(&value.syntax, name, &definitions, false);
        }
        match &resolved_kind {
            UseKind::Selected(names) => {
                for name in names {
                    let definitions = self
                        .resolved
                        .import_definitions(self.resolved_id(value.syntax.id), name)
                        .to_vec();
                    self.add(&value.syntax, name, &definitions, true);
                }
            }
            UseKind::Renamed { item, alias } => {
                for name in [item, alias] {
                    let definitions = self
                        .resolved
                        .import_definitions(self.resolved_id(value.syntax.id), name)
                        .to_vec();
                    self.add(&value.syntax, name, &definitions, name == alias);
                }
            }
            UseKind::Dotted | UseKind::Namespace | UseKind::Glob => {}
        }
    }

    fn use_module_segments(&mut self, declaration: &UseDeclaration, kind: &UseKind) {
        let module_components = match (&declaration.kind, kind) {
            (UseKind::Dotted, UseKind::Selected(_)) => declaration.path.len().saturating_sub(1),
            _ => declaration.path.len(),
        };
        let components = &declaration.path[..module_components];
        let rooted = components
            .first()
            .is_some_and(|name| matches!(name.as_str(), "std" | "package"));
        if components.first().is_some_and(|name| name == "package")
            && let Some(root) = self.resolved.program().package_root()
        {
            self.add(
                &declaration.syntax,
                "package",
                &[DefinitionId::Module(root)],
                false,
            );
        }
        let Some(target) = self
            .resolved
            .program()
            .imported_module(self.resolved_id(declaration.syntax.id))
        else {
            return;
        };
        let target_path = &self.resolved.program().module(target).path;
        let physical_components = if components.first().is_some_and(|name| name == "package") {
            &components[1..]
        } else {
            components
        };
        let mut root = target_path.as_path();
        for _ in physical_components {
            let Some(parent) = root.parent() else { return };
            root = parent;
        }

        for index in usize::from(rooted)..components.len() {
            let physical_end = if components.first().is_some_and(|name| name == "package") {
                index
            } else {
                index + 1
            };
            let mut relative = PathBuf::new();
            for component in &physical_components[..physical_end] {
                relative.push(component);
            }
            relative.set_extension("sta");
            let path = root.join(relative);
            if !path.is_file() {
                continue;
            }
            let name = &components[index];
            if let Some(module) = self
                .resolved
                .program()
                .modules()
                .iter()
                .find(|module| module.parent.is_none() && module.path == path)
            {
                self.add(
                    &declaration.syntax,
                    name,
                    &[DefinitionId::Module(module.id)],
                    false,
                );
            } else if let Some(range) = crate::staple_lsp::source_projection::named_range(
                &declaration.syntax,
                name,
                false,
                self.path,
            ) {
                self.entries.push(DefinitionEntry {
                    range,
                    targets: vec![DefinitionTarget {
                        path,
                        range: 0..0,
                        selection_range: 0..0,
                    }],
                });
            }
        }
    }

    /// Go-to-definition for `use` path segments without a backing `.sta` file:
    /// an inline `mod`, or a `companion` block (`Switch` in `use Switch.*`,
    /// which jumps to the type declaration). File-backed segments are already
    /// handled by `use_module_segments`.
    fn inline_use_path_segments(&mut self, declaration: &UseDeclaration, kind: &UseKind) {
        let module_components = match (&declaration.kind, kind) {
            (UseKind::Dotted, UseKind::Selected(_)) => declaration.path.len().saturating_sub(1),
            _ => declaration.path.len(),
        };
        let components = &declaration.path[..module_components];
        if components
            .first()
            .is_some_and(|name| matches!(name.as_str(), "std" | "package" | "super"))
        {
            return;
        }
        let program = self.resolved.program();
        let Some(target) = program.imported_module(self.resolved_id(declaration.syntax.id)) else {
            return;
        };
        let mut ancestors = vec![target];
        while let Some(parent) = program.module(*ancestors.last().unwrap()).parent {
            ancestors.push(parent);
        }
        for (index, name) in components.iter().enumerate() {
            let Some(&module) = ancestors.get(components.len() - 1 - index) else {
                continue;
            };
            let file_backed = program
                .module(module)
                .parent
                .is_none_or(|parent| program.module(module).path != program.module(parent).path);
            match self.resolved.companion_type_for_module(module) {
                Some(ty) => self.add(&declaration.syntax, name, &[DefinitionId::Type(ty)], false),
                None if !file_backed => self.add(
                    &declaration.syntax,
                    name,
                    &[DefinitionId::Module(module)],
                    false,
                ),
                None => {}
            }
        }
    }

    fn block_item(&mut self, item: &Item) {
        match item {
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
                if let Some(id) = self
                    .resolved
                    .program()
                    .child_module(self.resolved_id(value.syntax.id))
                {
                    let definitions = self.namespace_definitions(id);
                    self.add(&value.syntax, &value.name, &definitions, false);
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
            Expression::StringTemplate(value) => {
                for part in &value.parts {
                    if let StringTemplatePart::Interpolation(value) = part {
                        self.expression(&value.expression);
                    }
                }
            }
            Expression::Call(value) => {
                self.expression(&value.callee);
                self.expression(&value.argument);
            }
            Expression::Access(value) => {
                if let Accessor::Name(name) | Accessor::Method(name) = &value.accessor {
                    let definitions = self.definitions_for(value.syntax.id);
                    self.add(&value.syntax, name, &definitions, true);
                    self.qualified_receiver(&value.value);
                } else {
                    self.expression(&value.value);
                }
            }
            Expression::Index(value) => {
                self.expression(&value.value);
                self.expression(&value.index);
            }
            Expression::Logical(value) => {
                self.expression(&value.left);
                self.expression(&value.right);
                self.ty(&value.bool_type);
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
            }
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
            TypeParameterPattern::Effect(value) => {
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
                for resource in &value.effects.resources {
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

    /// Adds go-to-definition entries for the namespace/type segments of a
    /// resolved qualified access chain's receiver, e.g. `std` and `io` in
    /// `std.io.println`, or `List` in `List.push` (which points at the type
    /// declaration rather than the companion submodule). Segments that
    /// aren't part of a resolved qualified path (e.g. ordinary field
    /// access) fall back to the regular expression walk so nested content
    /// is still visited.
    fn qualified_receiver(&mut self, expression: &Expression) {
        match expression {
            Expression::Name(name) => {
                if let Some(module) = self
                    .resolved
                    .namespace_for(self.resolved_id(name.syntax.id))
                {
                    let definitions = self.namespace_definitions(module);
                    self.add(&name.syntax, &name.name, &definitions, false);
                } else {
                    self.expression(expression);
                }
            }
            Expression::Access(access) => {
                if let Accessor::Name(member) = &access.accessor
                    && let Some(module) = self
                        .resolved
                        .namespace_for(self.resolved_id(access.syntax.id))
                {
                    self.qualified_receiver(&access.value);
                    let definitions = self.namespace_definitions(module);
                    self.add(&access.syntax, member, &definitions, false);
                } else {
                    self.expression(expression);
                }
            }
            _ => self.expression(expression),
        }
    }

    fn namespace_definitions(&self, module: ModuleId) -> Vec<DefinitionId> {
        match self.resolved.companion_type_for_module(module) {
            Some(ty) => vec![DefinitionId::Type(ty)],
            None => vec![DefinitionId::Module(module)],
        }
    }

    fn add_resolved(&mut self, syntax: &Syntax, name: &str, last: bool) {
        let definitions = self.definitions_for(syntax.id);
        self.add(syntax, name, &definitions, last);
    }

    fn definitions_for(&self, syntax: SyntaxId) -> Vec<DefinitionId> {
        let syntax = self.resolved_id(syntax);
        let mut definitions = self.resolved.definitions_for(syntax);
        if let Some(symbol) = self.typed.and_then(|typed| typed.symbol_for(syntax)) {
            definitions.push(DefinitionId::Symbol(symbol));
        }
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
        let declaration = self
            .resolved
            .macro_invocation_for(self.resolved_id(syntax))?
            .declaration;
        Some(self.resolved.definitions_for(declaration))
    }

    fn add(&mut self, syntax: &Syntax, name: &str, definitions: &[DefinitionId], last: bool) {
        let Some(range) =
            crate::staple_lsp::source_projection::named_range(syntax, name, last, self.path)
        else {
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

fn token_range(syntax: &Syntax, name: &str, last: bool) -> Option<Range<usize>> {
    let mut tokens = syntax.tokens().iter().filter(|token| token.text == name);
    if last {
        tokens.next_back().map(|token| token.span.clone())
    } else {
        tokens.next().map(|token| token.span.clone())
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
            "def wrap: <T> move T -> Wrapper T = move value => Wrapper (value: value)\n",
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
    fn qualified_companion_access_targets_the_type_declaration() {
        let source = concat!(
            "type Box = I32\n",
            "companion Box {\n",
            "    pub def create = () => 1\n",
            "}\n",
            "let value = Box.create\n",
        );
        let path = std::env::temp_dir().join("staple-definition-companion-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        let type_declaration_offset = source.find("type Box").unwrap() + "type ".len();
        let companion_header_offset = source.find("companion Box").unwrap() + "companion ".len();
        assert_ne!(type_declaration_offset, companion_header_offset);

        let use_site = source.rfind("Box.create").unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.range.start == use_site && &source[entry.range.clone()] == "Box")
            .unwrap_or_else(|| panic!("no definition entry for the qualified `Box`: {entries:?}"));

        assert!(
            entry
                .targets
                .iter()
                .any(|target| target.selection_range.start == type_declaration_offset),
            "expected Box.create's `Box` to resolve to the type declaration: {entry:?}"
        );
        assert!(
            entry
                .targets
                .iter()
                .all(|target| target.selection_range.start != companion_header_offset),
            "Box.create's `Box` should not resolve to the companion block header: {entry:?}"
        );
    }

    #[test]
    fn use_glob_path_segment_targets_the_companion_type() {
        let source = concat!(
            "type Box = I32\n",
            "companion Box {\n",
            "    pub type Inner\n",
            "}\n",
            "use Box.*\n",
        );
        let path = std::env::temp_dir().join("staple-definition-use-companion-segment.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        let type_declaration_offset = source.find("type Box").unwrap() + "type ".len();
        let companion_header_offset = source.find("companion Box").unwrap() + "companion ".len();
        let segment = source.find("use Box.*").unwrap() + "use ".len();

        let entry = entries
            .iter()
            .find(|entry| entry.range.start == segment && &source[entry.range.clone()] == "Box")
            .unwrap_or_else(|| panic!("no definition entry for `Box` in `use Box.*`: {entries:?}"));

        assert!(
            entry
                .targets
                .iter()
                .any(|target| target.selection_range.start == type_declaration_offset),
            "expected `use Box.*`'s `Box` to resolve to the type declaration: {entry:?}"
        );
        assert!(
            entry
                .targets
                .iter()
                .all(|target| target.selection_range.start != companion_header_offset),
            "`use Box.*`'s `Box` should not resolve to the companion header: {entry:?}"
        );
    }

    #[test]
    fn companion_header_targets_the_type_declaration() {
        let source = concat!(
            "type Box = I32\n",
            "companion Box {\n",
            "    pub def create = () => 1\n",
            "}\n",
        );
        let path = std::env::temp_dir().join("staple-definition-companion-header-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        let type_declaration_offset = source.find("type Box").unwrap() + "type ".len();
        let companion_header_offset = source.find("companion Box").unwrap() + "companion ".len();

        let entry = entries
            .iter()
            .find(|entry| {
                entry.range.start == companion_header_offset
                    && &source[entry.range.clone()] == "Box"
            })
            .unwrap_or_else(|| {
                panic!("no definition entry for the `companion Box` header: {entries:?}")
            });

        assert!(
            entry
                .targets
                .iter()
                .any(|target| target.selection_range.start == type_declaration_offset),
            "expected the `companion Box` header to resolve to the type declaration: {entry:?}"
        );
        assert!(
            entry
                .targets
                .iter()
                .all(|target| target.selection_range.start != companion_header_offset),
            "the `companion Box` header should not resolve to itself: {entry:?}"
        );
    }

    #[test]
    fn qualified_trait_access_targets_the_trait_declaration() {
        let source = concat!(
            "trait ToString T { to_string: T -> String }\n",
            "impl ToString I32 { def to_string = value => \"\" }\n",
            "def f: I32 -> String = ToString.to_string\n",
        );
        let path = std::env::temp_dir().join("staple-definition-trait-access-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, typed.resolved(), Some(&typed));

        let declaration_offset = source.find("trait ToString").unwrap() + "trait ".len();

        let use_site = source.rfind("ToString.to_string").unwrap();
        let entry = entries
            .iter()
            .find(|entry| {
                entry.range.start == use_site && &source[entry.range.clone()] == "ToString"
            })
            .unwrap_or_else(|| {
                panic!("no definition entry for the qualified `ToString`: {entries:?}")
            });
        assert!(
            entry
                .targets
                .iter()
                .any(|target| target.selection_range.start == declaration_offset),
            "expected ToString.to_string's `ToString` to resolve to the trait declaration: {entry:?}"
        );
    }

    #[test]
    fn indexes_generic_trait_implementation_type_parameters() {
        let source = concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: move T -> T }\n",
            "impl Bound I32 { def check = value => True }\n",
            "impl <T where Bound T> Target T { def act = move value => value }\n",
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
            "@doc(\"A kept value.\")\n",
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
        let doc = entries
            .iter()
            .find(|entry| &source[entry.range.clone()] == "doc")
            .expect("expected a definition entry for `@doc`");
        assert!(
            doc.targets
                .iter()
                .any(|target| target.path.ends_with("std/core/doc.sta"))
        );
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
                "pub mod\n",
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
    fn indexes_file_module_segments_in_dotted_item_imports() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = "use std.io.println\n";
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, &root)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &resolved, None);
        let io = std::fs::canonicalize(root.join("stdlib/std/io.sta")).unwrap();

        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "io"
                    && entry.targets.iter().any(|target| target.path == io)
            }),
            "missing `io` module definition: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|entry| &source[entry.range.clone()] == "std")
        );

        // The imported item resolves to its own definition only, never also
        // to the whole `io` module (whose target range starts at the `pub mod`
        // line).
        let println_entries = entries
            .iter()
            .filter(|entry| &source[entry.range.clone()] == "println")
            .collect::<Vec<_>>();
        assert_eq!(println_entries.len(), 1, "entries: {entries:?}");
        assert_eq!(
            println_entries[0].targets.len(),
            1,
            "`println` should have a single definition target: {println_entries:?}"
        );
        assert!(
            println_entries[0]
                .targets
                .iter()
                .all(|target| target.selection_range != (0..0)),
            "`println` should not point at the `io` module header: {println_entries:?}"
        );
    }

    #[test]
    fn does_not_project_macro_definition_ranges_onto_call_site_tokens() {
        let source = concat!(
            "use std.io.println\n",
            "\n",
            "typegroup A {\n",
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
            "    when {\n",
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
        assert_target(&source, &entries, "when_clauses", "when_clauses");
        assert_target(&source, &entries, "body", "body");
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "IntoIterator"
                    && entry
                        .targets
                        .iter()
                        .any(|target| target.path.ends_with("iterator.sta"))
            }),
            "{entries:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "Expr"
                    && entry
                        .targets
                        .iter()
                        .any(|target| target.path.ends_with("syntax.sta"))
            }),
            "{entries:?}"
        );
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
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == keyword
                        && entry
                            .targets
                            .iter()
                            .any(|target| target.path.ends_with("syntax.sta"))
                }),
                "missing definition for {keyword}: {entries:?}"
            );
        }
    }

    #[test]
    fn package_definition_targets_the_configured_root_module() {
        let root = std::env::temp_dir().join(format!(
            "staple-definition-package-root-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let root_path = root.join("root.sta");
        std::fs::write(&root_path, "pub mod\npub let answer = 42\n").unwrap();
        let source = "use package.answer\nanswer\n";
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_module_root(&root)
            .with_package_root(&root_path)
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &resolved, None);
        let canonical_root = std::fs::canonicalize(&root_path).unwrap();
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "package"
                && entry
                    .targets
                    .iter()
                    .any(|target| target.path == canonical_root)
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qualified_access_segments_target_intermediate_package_modules() {
        let root = std::env::temp_dir().join(format!(
            "staple-definition-qualified-segments-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("outer")).unwrap();
        std::fs::write(root.join("outer.sta"), "pub mod\n").unwrap();
        std::fs::write(
            root.join("outer/inner.sta"),
            "pub mod\npub let answer = 42\n",
        )
        .unwrap();
        let source = "let value: I32 = package.outer.inner.answer\nvalue\n";
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_module_root(&root)
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &resolved, None);
        let canonical_outer = std::fs::canonicalize(root.join("outer.sta")).unwrap();
        let canonical_inner = std::fs::canonicalize(root.join("outer/inner.sta")).unwrap();
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "outer"
                    && entry
                        .targets
                        .iter()
                        .any(|target| target.path == canonical_outer)
            }),
            "missing outer segment: {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "inner"
                    && entry
                        .targets
                        .iter()
                        .any(|target| target.path == canonical_inner)
            }),
            "missing inner segment: {entries:?}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qualified_segments_do_not_gain_shifted_targets_under_a_package_graph() {
        // When the open file is not parsed first (a package graph parses the
        // package-root module ahead of it), the editor's own `parse` produces
        // `SyntaxId`s shifted from the loaded program's. Walking that copy made
        // every qualified-access segment pick up a neighbouring node's
        // definition on top of its real one.
        let root = std::env::temp_dir().join(format!(
            "staple-definition-package-graph-shift-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/middle")).unwrap();
        std::fs::write(
            root.join("binder.kdl"),
            "package \"hello_world\" {\n    root \"src/root.sta\"\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/root.sta"), "/// Test docs\npub mod").unwrap();
        std::fs::write(
            root.join("src/utils.sta"),
            "/// utils\npub mod\n\n/// add\npub def add = a: I32 => b: I32 => a + b\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/middle/foo.sta"),
            "pub mod\n\nmod aa {\n    use super.bar\n\n    let a = bar ()\n}\n\npub def bar = () => ()\n",
        )
        .unwrap();
        let source = concat!(
            "use std.io.println\n",
            "use package.utils.add\n",
            "\n",
            "pub type Foo = ()\n",
            "\n",
            "companion Foo {\n",
            "    def a = x: I32 => {}\n",
            "}\n",
            "\n",
            "package.middle.foo.bar ()\n",
            "package.utils.add 1 2\n",
        );
        std::fs::write(root.join("src/main.sta"), source).unwrap();
        let path = std::fs::canonicalize(root.join("src/main.sta")).unwrap();
        let graph = binder::load_package_graph(&root.join("binder.kdl")).unwrap();
        let program = ProgramLoader::new()
            .with_package_graph(graph)
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_package_graph_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries_at_path(&path, &module, typed.resolved(), Some(&typed));
        std::fs::remove_dir_all(&root).unwrap();

        let targets_for = |origin: &str, offset: usize| {
            entries
                .iter()
                .find(|entry| entry.range.start >= offset && &source[entry.range.clone()] == origin)
                .map(|entry| entry.targets.clone())
                .unwrap_or_default()
        };

        let line_11 = source.find("package.utils.add 1 2").unwrap();
        let line_10 = source.find("package.middle.foo.bar ()").unwrap();

        for (origin, offset, expected) in [
            ("utils", line_11, "utils.sta"),
            ("add", line_11, "utils.sta"),
            ("foo", line_10, "foo.sta"),
        ] {
            let targets = targets_for(origin, offset);
            assert_eq!(
                targets.len(),
                1,
                "`{origin}` should resolve to exactly one definition, got {targets:?}"
            );
            assert!(
                targets[0].path.ends_with(expected),
                "`{origin}` should resolve into {expected}, got {targets:?}"
            );
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
