use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use stapler::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverEntry {
    pub range: Range<usize>,
    pub signature: String,
    pub documentation: Vec<String>,
}

#[cfg(test)]
pub fn entries(module: &Module, typed: &TypedModule) -> Vec<HoverEntry> {
    let resolved = typed.resolved();
    let path = &resolved.program().module(resolved.program().entry()).path;
    entries_at_path(path, module, typed)
}

pub fn entries_at_path(path: &Path, module: &Module, typed: &TypedModule) -> Vec<HoverEntry> {
    let mut collector = Collector {
        typed,
        entries: Vec::new(),
        declarations: HashMap::new(),
        path,
    };
    for source_module in typed.resolved().program().modules() {
        collector.collect_module_declarations(&source_module.syntax);
    }
    // Also scan the editor-owned syntax, which retains unexpanded source IDs.
    collector.collect_module_declarations(module);
    for item in &module.items {
        collector.item(item);
    }
    for item in &typed.resolved().syntax().items {
        collector.item(item);
    }
    collector.entries.sort_by(|left, right| {
        left.range
            .start
            .cmp(&right.range.start)
            .then(left.range.end.cmp(&right.range.end))
            .then(left.signature.cmp(&right.signature))
            .then(left.documentation.cmp(&right.documentation))
    });
    collector.entries.dedup();
    collector.entries
}

fn module_docs_from_file(path: &Path) -> Vec<String> {
    let Ok(source) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(module) = parse(&source) else {
        return Vec::new();
    };
    module
        .modifiers
        .iter()
        .filter(|modifier| modifier.namespace.is_none() && modifier.name == "doc")
        .filter_map(|modifier| {
            if let Some(doc) = &modifier.doc {
                return Some(doc.clone());
            }
            let Expression::String(literal) = modifier.argument.as_ref()?.expression.as_ref()?
            else {
                return None;
            };
            let text = literal.literal.strip_prefix('"')?.strip_suffix('"')?;
            Some(
                text.replace("\\n", "\n")
                    .replace("\\r", "\r")
                    .replace("\\t", "\t")
                    .replace("\\\"", "\"")
                    .replace("\\\\", "\\"),
            )
        })
        .collect()
}

struct Collector<'a> {
    typed: &'a TypedModule,
    entries: Vec<HoverEntry>,
    declarations: HashMap<SymbolId, Declaration>,
    path: &'a Path,
}

#[derive(Clone)]
struct Declaration {
    prefix: Option<String>,
    name: String,
    docs: Vec<String>,
}

impl Collector<'_> {
    fn collect_module_declarations(&mut self, module: &Module) {
        for item in &module.items {
            self.collect_item_declarations(item);
        }
    }

    fn collect_item_declarations(&mut self, item: &Item) {
        match item {
            Item::Modified(value) => {
                for modifier in &value.modifiers {
                    if let Some(info) = self
                        .typed
                        .resolved()
                        .macro_invocation_for(modifier.syntax.id)
                    {
                        self.named_last_with_docs(
                            &modifier.syntax,
                            &modifier.name,
                            macro_signature(info),
                            info.docs.clone(),
                        );
                    }
                    if let Some(expression) = modifier
                        .argument
                        .as_ref()
                        .and_then(|argument| argument.expression.as_ref())
                    {
                        self.collect_expression_declarations(expression);
                    }
                }
                self.collect_item_declarations(&value.item);
            }
            Item::VisibilityMacroInvocation(value) => {
                for modifier in &value.modifiers {
                    if let Some(expression) = modifier
                        .argument
                        .as_ref()
                        .and_then(|argument| argument.expression.as_ref())
                    {
                        self.collect_expression_declarations(expression);
                    }
                }
                self.collect_expression_declarations(&value.expression)
            }
            Item::VisibilitySplice(value) => self.collect_item_declarations(&value.item),
            Item::RepeatedItemSplice(_) => {}
            Item::Submodule(submodule) => self.collect_module_declarations(&submodule.module),
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    self.collect_binding_declaration(binding);
                }
            }
            Item::MacroDeclaration(declaration) => {
                if let Some(value) = &declaration.value {
                    self.collect_expression_declarations(value);
                }
            }
            Item::TraitDeclaration(declaration) => {
                for member in &declaration.members {
                    if let Some(default) = &member.default {
                        self.collect_expression_declarations(default);
                    }
                }
            }
            Item::TraitImplementation(implementation) => {
                for member in &implementation.members {
                    self.collect_expression_declarations(&member.value);
                }
            }
            item @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.collect_block_item_declarations(item),
            Item::UseDeclaration(_) | Item::TypeDeclaration(_) => {}
        }
    }

    fn collect_block_item_declarations(&mut self, item: &Item) {
        match item {
            Item::Binding(binding) => self.collect_binding_declaration(binding),
            Item::PatternBinding(binding) => {
                self.collect_pattern_declarations(&binding.pattern, true);
                self.collect_expression_declarations(&binding.value);
            }
            Item::Assignment(assignment) => {
                self.collect_expression_declarations(&assignment.target);
                self.collect_expression_declarations(&assignment.value);
            }
            Item::Return(return_) => self.collect_expression_declarations(&return_.value),
            Item::Break(break_) => {
                if let Some(value) = &break_.value {
                    self.collect_expression_declarations(value);
                }
            }
            Item::Continue(_) => {}
            Item::Expression(expression) => self.collect_expression_declarations(expression),
            Item::Submodule(submodule) => self.collect_module_declarations(&submodule.module),
            Item::TypeDeclaration(_) => {}
            Item::UseDeclaration(_) => {}
            _ => {}
        }
    }

    fn collect_binding_declaration(&mut self, binding: &Binding) {
        if let Some(symbol) = self.typed.symbol_for(binding.syntax.id) {
            let docs = if binding.docs.is_empty() {
                self.declarations
                    .get(&symbol)
                    .map(|declaration| declaration.docs.clone())
                    .unwrap_or_default()
            } else {
                binding.docs.clone()
            };
            self.declarations.insert(
                symbol,
                Declaration {
                    prefix: Some(binding.declaration_prefix()),
                    name: binding.name.clone(),
                    docs,
                },
            );
        }
        if let Some(value) = &binding.value {
            self.collect_expression_declarations(value);
        }
    }

    fn collect_expression_declarations(&mut self, expression: &Expression) {
        match expression {
            Expression::Function(function) => {
                self.collect_pattern_declarations(&function.pattern, false);
                self.collect_expression_declarations(&function.body);
            }
            Expression::Satisfies(satisfies) => {
                self.collect_expression_declarations(&satisfies.value)
            }
            Expression::Match(match_) => {
                self.collect_expression_declarations(&match_.subject);
                for arm in &match_.arms {
                    self.collect_pattern_declarations(&arm.pattern, false);
                    self.collect_expression_declarations(&arm.body);
                }
            }
            Expression::Loop(loop_) => {
                for item in &loop_.body.items {
                    self.collect_item_declarations(item);
                }
            }
            Expression::Resource(_) => {}
            Expression::With(with) => {
                self.collect_expression_declarations(&with.value);
                for item in &with.body.items {
                    self.collect_item_declarations(item);
                }
            }
            Expression::Block(block) => {
                for item in &block.items {
                    self.collect_item_declarations(item);
                }
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    self.collect_expression_declarations(&element.value);
                }
            }
            Expression::StringTemplate(value) => {
                for part in &value.parts {
                    if let StringTemplatePart::Interpolation(value) = part {
                        self.collect_expression_declarations(&value.expression);
                    }
                }
            }
            Expression::Call(call) => {
                self.collect_expression_declarations(&call.callee);
                self.collect_expression_declarations(&call.argument);
            }
            Expression::Access(access) => self.collect_expression_declarations(&access.value),
            Expression::Index(index) => {
                self.collect_expression_declarations(&index.value);
                self.collect_expression_declarations(&index.index);
            }
            Expression::Logical(logical) => {
                self.collect_expression_declarations(&logical.left);
                self.collect_expression_declarations(&logical.right);
            }
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
            Expression::Quote(quote) => match &quote.template {
                QuoteTemplate::Expression(expression) => {
                    self.collect_expression_declarations(expression)
                }
                QuoteTemplate::Item(item) => self.collect_item_declarations(item),
                QuoteTemplate::Items(items) => items
                    .iter()
                    .for_each(|item| self.collect_item_declarations(item)),
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

    fn collect_pattern_declarations(&mut self, pattern: &Pattern, is_let_context: bool) {
        match pattern {
            Pattern::At(at) => {
                self.collect_pattern_declarations(
                    &Pattern::Binding(at.binding.as_ref().clone()),
                    is_let_context,
                );
                self.collect_pattern_declarations(&at.pattern, is_let_context);
            }
            Pattern::Binding(binding) => {
                if let Some(symbol) = self.typed.symbol_for(binding.syntax.id) {
                    // A binding inside a whole-`mut`-marked product pattern
                    // (`mut (x, y) => ...`) carries the mutation permission
                    // on the enclosing `Pattern::Product`, not on itself, so
                    // `binding.mutable` alone misses it — fall back to the
                    // checked parameter-mutation set, which already
                    // flattens a whole marker onto every destructured
                    // element.
                    let prefix = if binding.mutable || self.typed.is_mutated_parameter(symbol) {
                        Some("mut".to_owned())
                    } else if is_let_context {
                        Some("let".to_owned())
                    } else {
                        None
                    };
                    self.declarations.insert(
                        symbol,
                        Declaration {
                            prefix,
                            name: binding.name.clone(),
                            docs: Vec::new(),
                        },
                    );
                }
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.collect_pattern_declarations(element, is_let_context);
                }
            }
            Pattern::Nominal(nominal) => {
                self.collect_pattern_declarations(&nominal.argument, is_let_context)
            }
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
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
            Item::Submodule(submodule) => {
                // A `companion` header names the type it extends, so it hovers
                // as the type declaration rather than as `mod Switch`.
                let companion_type = submodule.companion.then(|| {
                    let resolved = self.typed.resolved();
                    resolved
                        .program()
                        .child_module(submodule.syntax.id)
                        .and_then(|module| resolved.companion_type_for_module(module))
                });
                let companion_signature = companion_type
                    .flatten()
                    .and_then(|ty| Some((ty, self.type_signature(ty, submodule.syntax.id)?)));
                if let Some((ty, signature)) = companion_signature {
                    let docs = self
                        .typed
                        .resolved()
                        .type_declarations()
                        .get(&ty)
                        .map(|declaration| declaration.docs.clone())
                        .unwrap_or_default();
                    self.named_with_docs(&submodule.syntax, &submodule.name, signature, docs);
                } else {
                    let docs = self
                        .typed
                        .resolved()
                        .program()
                        .modules()
                        .iter()
                        .flat_map(|module| &module.syntax.items)
                        .find_map(|item| match item {
                            Item::Submodule(resolved)
                                if resolved.syntax.id == submodule.syntax.id =>
                            {
                                Some(resolved.docs.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| submodule.docs.clone());
                    self.named_with_docs(
                        &submodule.syntax,
                        &submodule.name,
                        format!("mod {}", submodule.name),
                        docs,
                    );
                }
                for item in &submodule.module.items {
                    self.item(item);
                }
            }
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    self.binding(binding);
                }
            }
            Item::MacroDeclaration(declaration) => {
                if let Some(info) = self
                    .typed
                    .resolved()
                    .macro_definition_for(declaration.syntax.id)
                {
                    self.named_with_docs(
                        &declaration.syntax,
                        &declaration.name,
                        macro_signature(info),
                        info.docs.clone(),
                    );
                }
                for parameter in &declaration.type_parameters {
                    self.type_parameter(parameter);
                }
                for bound in &declaration.trait_bounds {
                    self.trait_bound(bound);
                }
                if let Some(annotation) = &declaration.annotation {
                    self.ty(annotation);
                }
                if let Some(value) = &declaration.value {
                    self.expression(value);
                }
            }
            Item::TraitImplementation(implementation) => {
                for parameter in &implementation.type_parameters {
                    self.type_parameter(parameter);
                }
                for bound in &implementation.trait_bounds {
                    self.trait_bound(bound);
                }
                for argument in &implementation.arguments {
                    self.ty(argument);
                }
                for member in &implementation.members {
                    let value_type = self
                        .typed
                        .type_of_expression(member.value.syntax().id)
                        .map(|value_type| self.display_type(value_type))
                        .or_else(|| self.implementation_member_type(implementation, member));
                    if let Some(value_type) = value_type {
                        self.named_with_docs(
                            &member.syntax,
                            &member.name,
                            format!("def {}: {value_type}", member.name),
                            member.docs.clone(),
                        );
                    }
                    self.expression(&member.value);
                }
            }
            Item::TraitDeclaration(declaration) => {
                let docs = self
                    .typed
                    .resolved()
                    .traits()
                    .values()
                    .find(|resolved| resolved.declaration.syntax.id == declaration.syntax.id)
                    .map(|resolved| resolved.declaration.docs.clone())
                    .unwrap_or_else(|| declaration.docs.clone());
                let (parameters, where_clause) = self.juxtaposed_generic_suffix(
                    &declaration.type_parameters,
                    &declaration.prerequisites,
                    &declaration.subtype_bounds,
                    &declaration.functional_dependencies,
                );
                self.named_with_docs(
                    &declaration.syntax,
                    &declaration.name,
                    format!("trait {}{parameters}{where_clause}", declaration.name),
                    docs,
                );
                for parameter in &declaration.type_parameters {
                    self.type_parameter(parameter);
                }
                for dependency in &declaration.functional_dependencies {
                    for determinant in &dependency.determinants {
                        self.ty(&Type::Named(determinant.clone()));
                    }
                    self.ty(&Type::Named(dependency.dependent.clone()));
                }
                for prerequisite in &declaration.prerequisites {
                    self.trait_bound(prerequisite);
                }
                for member in &declaration.members {
                    self.named_with_docs(
                        &member.syntax,
                        &member.name,
                        format!(
                            "<trait member> {}: {}",
                            member.name,
                            member.annotation.syntax().text().trim()
                        ),
                        member.docs.clone(),
                    );
                    self.ty(&member.annotation);
                    if let Some(default) = &member.default {
                        self.expression(default);
                    }
                }
            }
            Item::TypeDeclaration(declaration) => self.type_declaration(declaration),
            item @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.block_item(item),
            Item::UseDeclaration(declaration) => self.use_declaration(declaration),
        }
    }

    fn use_declaration(&mut self, declaration: &UseDeclaration) {
        let resolved_kind = self
            .typed
            .resolved()
            .program()
            .use_kind(declaration)
            .clone();
        self.imported_module_segments(declaration, &resolved_kind);
        self.inline_use_path_segments(declaration, &resolved_kind);
        match &resolved_kind {
            UseKind::Selected(names) => {
                for name in names {
                    self.imported_name(declaration, &name, &name, false);
                }
            }
            UseKind::Renamed { item, alias } => {
                self.imported_name(declaration, &item, &item, false);
                self.imported_name(declaration, &alias, &item, true);
            }
            UseKind::Dotted | UseKind::Namespace | UseKind::Glob => {}
        }
    }

    /// Annotates module segments of a `use` path that `imported_module_segments`
    /// can't, because they name something without a backing `.sta` file: an
    /// inline `mod`, or a `companion` block (`Switch` in `use Switch.*`, which
    /// hovers as the type it extends).
    fn inline_use_path_segments(&mut self, declaration: &UseDeclaration, kind: &UseKind) {
        let Some(chain) = self.use_path_module_chain(declaration, kind) else {
            return;
        };
        for (name, module, file_backed) in chain {
            let resolved = self.typed.resolved();
            if let Some(ty) = resolved.companion_type_for_module(module) {
                if let Some(signature) = self.type_signature(ty, declaration.syntax.id) {
                    let docs = self
                        .typed
                        .resolved()
                        .type_declarations()
                        .get(&ty)
                        .map(|entry| entry.docs.clone())
                        .unwrap_or_default();
                    self.named_with_docs(&declaration.syntax, &name, signature, docs);
                }
            } else if !file_backed {
                let docs = self.submodule_docs(module);
                self.named_with_docs(&declaration.syntax, &name, format!("mod {name}"), docs);
            }
        }
    }

    /// Aligns the module segments of a non-rooted `use` path with the module
    /// each one names, by walking up from the resolved target module. Yields
    /// `(segment name, module, is file-backed)`; file-backed segments are
    /// already handled by `imported_module_segments`.
    fn use_path_module_chain(
        &self,
        declaration: &UseDeclaration,
        kind: &UseKind,
    ) -> Option<Vec<(String, ModuleId, bool)>> {
        let module_components = match (&declaration.kind, kind) {
            (UseKind::Dotted, UseKind::Selected(_)) => declaration.path.len().saturating_sub(1),
            _ => declaration.path.len(),
        };
        let components = &declaration.path[..module_components];
        if components
            .first()
            .is_some_and(|name| matches!(name.as_str(), "std" | "package" | "super"))
        {
            return None;
        }
        let program = self.typed.resolved().program();
        let target = program.imported_module(declaration.syntax.id)?;
        let mut ancestors = vec![target];
        while let Some(parent) = program.module(*ancestors.last().unwrap()).parent {
            ancestors.push(parent);
        }
        Some(
            components
                .iter()
                .enumerate()
                .filter_map(|(index, name)| {
                    let module = *ancestors.get(components.len() - 1 - index)?;
                    let file_backed = program.module(module).parent.is_none_or(|parent| {
                        program.module(module).path != program.module(parent).path
                    });
                    Some((name.clone(), module, file_backed))
                })
                .collect(),
        )
    }

    /// Documentation on the `mod`/`companion` declaration that introduces the
    /// given child module, if any.
    fn submodule_docs(&self, child: ModuleId) -> Vec<String> {
        let resolved = self.typed.resolved();
        resolved
            .program()
            .modules()
            .iter()
            .flat_map(|source| &source.syntax.items)
            .find_map(|item| match item {
                Item::Submodule(submodule)
                    if resolved.program().child_module(submodule.syntax.id) == Some(child) =>
                {
                    Some(submodule.docs.clone())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    fn imported_module_segments(&mut self, declaration: &UseDeclaration, kind: &UseKind) {
        let module_components = match (&declaration.kind, kind) {
            (UseKind::Dotted, UseKind::Selected(_)) => declaration.path.len().saturating_sub(1),
            _ => declaration.path.len(),
        };
        let components = &declaration.path[..module_components];
        let rooted = components
            .first()
            .is_some_and(|name| matches!(name.as_str(), "std" | "package"));
        if rooted {
            let name = &components[0];
            let program = self.typed.resolved().program();
            let (signature, docs) = if name == "package" {
                let docs = program
                    .package_root()
                    .map(|root| program.module(root).syntax.docs.clone())
                    .unwrap_or_default();
                (format!("package {}", program.package_name()), docs)
            } else {
                (format!("package {name}"), Vec::new())
            };
            self.named_with_docs(&declaration.syntax, name, signature, docs);
        }

        let Some(target) = self
            .typed
            .resolved()
            .program()
            .imported_module(declaration.syntax.id)
        else {
            return;
        };
        let target_path = &self.typed.resolved().program().module(target).path;
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
            let docs = self
                .typed
                .resolved()
                .program()
                .modules()
                .iter()
                .find(|module| module.parent.is_none() && module.path == path)
                .map(|module| module.syntax.docs.clone())
                .unwrap_or_else(|| module_docs_from_file(&path));
            let name = &components[index];
            self.named_with_docs(&declaration.syntax, name, format!("mod {name}"), docs);
        }
    }

    fn imported_name(
        &mut self,
        declaration: &UseDeclaration,
        token_name: &str,
        imported_name: &str,
        last: bool,
    ) {
        let signature = self
            .typed
            .resolved()
            .import_definitions(declaration.syntax.id, imported_name)
            .iter()
            .find_map(|definition| {
                self.definition_signature_and_docs(*definition, declaration.syntax.id)
            });
        if let Some((signature, docs)) = signature {
            if last {
                self.named_last_with_docs(&declaration.syntax, token_name, signature, docs);
            } else {
                self.named_with_docs(&declaration.syntax, token_name, signature, docs);
            }
        }
    }

    fn definition_signature_and_docs(
        &self,
        definition: DefinitionId,
        from_syntax: SyntaxId,
    ) -> Option<(String, Vec<String>)> {
        let resolved = self.typed.resolved();
        match definition {
            DefinitionId::Symbol(symbol) => {
                let value_type = self
                    .typed
                    .type_of_symbol(symbol)
                    .map(|ty| self.display_type(ty))?;
                let declaration = self.declarations.get(&symbol);
                let signature = declaration
                    .map(|declaration| declaration.signature(&value_type))
                    .unwrap_or(value_type);
                let docs = declaration
                    .map(|declaration| declaration.docs.clone())
                    .unwrap_or_default();
                Some((signature, docs))
            }
            DefinitionId::Type(id) => {
                let signature = self.type_signature(id, from_syntax)?;
                let docs = resolved
                    .type_declarations()
                    .get(&id)
                    .map(|declaration| declaration.docs.clone())
                    .unwrap_or_default();
                Some((signature, docs))
            }
            DefinitionId::Trait(id) => resolved.traits().get(&id).map(|resolved| {
                let declaration = &resolved.declaration;
                let (parameters, where_clause) = self.juxtaposed_generic_suffix(
                    &declaration.type_parameters,
                    &declaration.prerequisites,
                    &declaration.subtype_bounds,
                    &declaration.functional_dependencies,
                );
                (
                    format!("trait {}{parameters}{where_clause}", declaration.name),
                    declaration.docs.clone(),
                )
            }),
            DefinitionId::TraitMethod(id) => resolved.trait_method(id).map(|member| {
                (
                    format!(
                        "<trait member> {}: {}",
                        member.name,
                        member.annotation.syntax().text().trim()
                    ),
                    member.docs.clone(),
                )
            }),
            DefinitionId::Macro(id) => resolved
                .macro_for(id)
                .map(|info| (macro_signature(info), info.docs.clone())),
            DefinitionId::CompileTime(syntax) => resolved
                .compile_time_binding_for(syntax)
                .map(|info| (compile_time_signature(info), Vec::new())),
            DefinitionId::TypeParameter(_) | DefinitionId::Module(_) => None,
        }
    }

    fn type_declaration(&mut self, declaration: &TypeDeclaration) {
        if let Some((id, _)) = self
            .typed
            .resolved()
            .type_declarations()
            .iter()
            .find(|(_, candidate)| candidate.syntax.id == declaration.syntax.id)
            && let Some(signature) = self.type_signature(*id, declaration.syntax.id)
        {
            let docs = self
                .typed
                .resolved()
                .type_declarations()
                .get(id)
                .map(|resolved| resolved.docs.clone())
                .unwrap_or_else(|| declaration.docs.clone());
            self.named_with_docs(&declaration.syntax, &declaration.name, signature, docs);
        }
        for parameter in &declaration.type_parameters {
            self.type_parameter(parameter);
        }
        for bound in &declaration.trait_bounds {
            self.trait_bound(bound);
        }
        if let Some(underlying) = &declaration.underlying {
            self.ty(underlying);
        }
    }

    /// Source text of each type parameter, e.g. `T` or `(Spelling = String)`
    /// for a defaulted one (the default is part of the parameter's own
    /// syntax span, so it comes along for free).
    fn parameter_names(&self, type_parameters: &[TypeParameterPattern]) -> Vec<String> {
        type_parameters
            .iter()
            .map(|parameter| parameter.syntax().text().trim().to_owned())
            .collect()
    }

    /// Builds the unified `where` clause text (trait bounds, subtype
    /// bounds, and — for traits — functional dependencies), e.g.
    /// ` where Debug T, T <: Super`, or an empty string when there are none.
    fn where_clause(
        &self,
        trait_bounds: &[TraitBound],
        subtype_bounds: &[SubtypeBound],
        functional_dependencies: &[FunctionalDependency],
    ) -> String {
        let constraints = functional_dependencies
            .iter()
            .map(|dependency| dependency.syntax.text().trim().to_owned())
            .chain(
                subtype_bounds
                    .iter()
                    .map(|bound| bound.syntax.text().trim().to_owned()),
            )
            .chain(
                trait_bounds
                    .iter()
                    .map(|bound| bound.syntax.text().trim().to_owned()),
            )
            .collect::<Vec<_>>();
        if constraints.is_empty() {
            String::new()
        } else {
            format!(" where {}", constraints.join(", "))
        }
    }

    /// The bracketed generic-parameter prefix used by `let`/`def`, `impl`,
    /// and `macro` signatures, e.g. `<T where Debug T> `, or an empty
    /// string when there are no type parameters.
    fn bracketed_generic_prefix(
        &self,
        type_parameters: &[TypeParameterPattern],
        trait_bounds: &[TraitBound],
        subtype_bounds: &[SubtypeBound],
    ) -> String {
        if type_parameters.is_empty() {
            return String::new();
        }
        let parameters = self.parameter_names(type_parameters).join(", ");
        let where_clause = self.where_clause(trait_bounds, subtype_bounds, &[]);
        format!("<{parameters}{where_clause}> ")
    }

    /// The juxtaposed generic-parameter suffix used by `type`/`trait`
    /// signatures: bare params directly after the name, then a `where`
    /// clause. Returns `(" T U", " where Bound T")`, either half empty when
    /// there's nothing to show.
    fn juxtaposed_generic_suffix(
        &self,
        type_parameters: &[TypeParameterPattern],
        trait_bounds: &[TraitBound],
        subtype_bounds: &[SubtypeBound],
        functional_dependencies: &[FunctionalDependency],
    ) -> (String, String) {
        let parameters = self.parameter_names(type_parameters).join(" ");
        let parameters = if parameters.is_empty() {
            String::new()
        } else {
            format!(" {parameters}")
        };
        let where_clause = self.where_clause(trait_bounds, subtype_bounds, functional_dependencies);
        (parameters, where_clause)
    }

    fn type_signature(&self, id: TypeId, from_syntax: SyntaxId) -> Option<String> {
        let resolved = self.typed.resolved();
        let declaration = resolved.type_declarations().get(&id)?;
        let from_module = resolved
            .module_for_syntax(from_syntax)
            .unwrap_or_else(|| resolved.program().entry());
        let representation_is_visible = declaration.kind == TypeDeclarationKind::Alias
            || resolved.representation_visible_from(id, from_module);
        let alias = if declaration.kind == TypeDeclarationKind::Alias {
            " alias"
        } else {
            ""
        };
        let (parameters, where_clause) = self.juxtaposed_generic_suffix(
            &declaration.type_parameters,
            &declaration.trait_bounds,
            &declaration.subtype_bounds,
            &[],
        );
        if !representation_is_visible {
            return Some(format!(
                "type{alias} {}{parameters}{where_clause}",
                declaration.name
            ));
        }
        let representation = declaration
            .underlying
            .as_ref()
            .map(|ty| ty.to_string())
            .unwrap_or_else(|| match declaration.kind {
                TypeDeclarationKind::Opaque => "opaque".to_owned(),
                TypeDeclarationKind::Singleton => "()".to_owned(),
                TypeDeclarationKind::Alias | TypeDeclarationKind::Distinct => "...".to_owned(),
            });
        Some(format!(
            "type{alias} {}{parameters}{where_clause} = {representation}",
            declaration.name
        ))
    }

    /// Hovers over the namespace/type segments of a resolved qualified
    /// access chain's receiver, e.g. `std` and `io` in `std.io.println`, or
    /// `List` in `List.push` (a companion access: hovers as the owning
    /// type, not the companion submodule).
    fn qualified_receiver(&mut self, expression: &Expression) {
        let resolved = self.typed.resolved();
        match expression {
            Expression::Name(name) if name.name == "package" => {
                // The `package` root hovers as the package itself, the same
                // as a `use package.…` path's leading segment.
                let program = resolved.program();
                let docs = program
                    .package_root()
                    .map(|root| program.module(root).syntax.docs.clone())
                    .unwrap_or_default();
                self.named_with_docs(
                    &name.syntax,
                    &name.name,
                    format!("package {}", program.package_name()),
                    docs,
                );
            }
            Expression::Name(name) => {
                if let Some(module) = resolved.namespace_for(name.syntax.id) {
                    self.namespace_segment(&name.syntax, &name.name, module);
                }
            }
            Expression::Access(access) => {
                self.qualified_receiver(&access.value);
                if let Accessor::Name(member) = &access.accessor
                    && let Some(module) = resolved.namespace_for(access.syntax.id)
                {
                    self.namespace_segment(&access.syntax, member, module);
                }
            }
            _ => {}
        }
    }

    fn namespace_segment(&mut self, syntax: &Syntax, name: &str, module: ModuleId) {
        let resolved = self.typed.resolved();
        if let Some(ty) = resolved.companion_type_for_module(module) {
            if let Some(signature) = self.type_signature(ty, syntax.id) {
                let docs = resolved
                    .type_declarations()
                    .get(&ty)
                    .map(|declaration| declaration.docs.clone())
                    .unwrap_or_default();
                self.named_last_with_docs(syntax, name, signature, docs);
            }
            return;
        }
        let docs = resolved
            .program()
            .modules()
            .iter()
            .flat_map(|source| &source.syntax.items)
            .find_map(|item| match item {
                Item::Submodule(submodule)
                    if resolved.program().child_module(submodule.syntax.id) == Some(module) =>
                {
                    Some(submodule.docs.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| resolved.program().module(module).syntax.docs.clone());
        self.named_last_with_docs(syntax, name, format!("mod {name}"), docs);
    }

    fn block_item(&mut self, item: &Item) {
        match item {
            Item::Binding(binding) => self.binding(binding),
            Item::PatternBinding(binding) => {
                self.pattern(&binding.pattern);
                self.expression(&binding.value);
            }
            Item::Assignment(assignment) => {
                self.expression(&assignment.target);
                self.expression(&assignment.value);
            }
            Item::Return(return_) => self.expression(&return_.value),
            Item::Break(break_) => {
                if let Some(value) = &break_.value {
                    self.expression(value);
                }
            }
            Item::Continue(_) => {}
            Item::Expression(expression) => self.expression(expression),
            Item::Submodule(submodule) => {
                for item in &submodule.module.items {
                    self.item(item);
                }
            }
            Item::TypeDeclaration(declaration) => self.type_declaration(declaration),
            Item::UseDeclaration(declaration) => self.use_declaration(declaration),
            _ => {}
        }
    }

    fn implementation_member_type(
        &self,
        implementation: &TraitImplementation,
        member: &ImplementationMember,
    ) -> Option<String> {
        let resolved = self.typed.resolved();
        let implementation = resolved
            .trait_implementations()
            .iter()
            .find(|resolved| resolved.syntax == implementation.syntax.id)?;
        let (_, function) = implementation.methods.iter().find(|(method, _)| {
            resolved
                .trait_method(**method)
                .is_some_and(|declaration| declaration.name == member.name)
        })?;
        self.typed
            .type_of_function(*function)
            .cloned()
            .map(CheckedType::Function)
            .map(|value_type| self.display_type(&value_type))
    }

    fn display_type(&self, value_type: &CheckedType) -> String {
        let resolved = self.typed.resolved();
        let mut names = resolved
            .type_declarations()
            .iter()
            .filter_map(|(id, declaration)| {
                let internal = resolved.type_name(*id)?;
                (internal != declaration.name).then_some((internal, declaration.name.as_str()))
            })
            .collect::<Vec<_>>();
        names.sort_unstable_by_key(|(internal, _)| std::cmp::Reverse(internal.len()));

        let mut displayed = value_type.to_string();
        for (internal, source) in names {
            displayed = displayed.replace(internal, source);
        }
        displayed
    }

    fn binding(&mut self, binding: &Binding) {
        if let Some(info) = self
            .typed
            .resolved()
            .compile_time_binding_for(binding.syntax.id)
        {
            self.named(&binding.syntax, &binding.name, compile_time_signature(info));
        }
        let value_type = self
            .typed
            .symbol_for(binding.syntax.id)
            .and_then(|symbol| self.typed.type_of_symbol(symbol))
            .or_else(|| {
                binding
                    .value
                    .as_ref()
                    .and_then(|value| self.typed.type_of_expression(value.syntax().id))
            })
            .map(|value_type| self.display_type(value_type));
        if let Some(value_type) = value_type {
            let prefix = binding.declaration_prefix();
            let generics = self.bracketed_generic_prefix(
                &binding.type_parameters,
                &binding.trait_bounds,
                &binding.subtype_bounds,
            );
            let docs = self
                .typed
                .symbol_for(binding.syntax.id)
                .and_then(|symbol| self.declarations.get(&symbol))
                .map(|declaration| declaration.docs.clone())
                .unwrap_or_else(|| binding.docs.clone());
            self.named_with_docs(
                &binding.syntax,
                &binding.name,
                format!("{prefix} {}: {generics}{value_type}", binding.name),
                docs,
            );
        }
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
        if let Some(info) = self
            .typed
            .resolved()
            .macro_invocation_for(expression.syntax().id)
        {
            match expression {
                Expression::Name(name) => self.named_with_docs(
                    &name.syntax,
                    &name.name,
                    macro_signature(info),
                    info.docs.clone(),
                ),
                Expression::Access(access) => {
                    if let Accessor::Name(name) = &access.accessor {
                        self.named_last_with_docs(
                            &access.syntax,
                            name,
                            macro_signature(info),
                            info.docs.clone(),
                        );
                    }
                }
                Expression::Quote(quote) => self.named_last_with_docs(
                    &quote.syntax,
                    quote.kind.name(),
                    macro_signature(info),
                    info.docs.clone(),
                ),
                _ => {}
            }
        }
        if let Some(info) = self
            .typed
            .resolved()
            .compile_time_binding_for(expression.syntax().id)
        {
            let signature = compile_time_signature(info);
            match expression {
                Expression::Name(name) => self.named(&name.syntax, &name.name, signature),
                Expression::Splice(splice) => self.named(&splice.syntax, &splice.name, signature),
                _ => {}
            }
        }
        if let Some(value_type) = self.typed.type_of_expression(expression.syntax().id) {
            let value_type = self.display_type(value_type);
            let declaration = self
                .typed
                .symbol_for(expression.syntax().id)
                .and_then(|symbol| self.declarations.get(&symbol));
            let trait_member = self
                .typed
                .resolved()
                .trait_methods_for_expression(expression.syntax().id)
                .first()
                .and_then(|method| self.typed.resolved().trait_method(*method));
            let signature = declaration
                .map(|declaration| declaration.signature(&value_type))
                .or_else(|| {
                    trait_member
                        .map(|member| format!("<trait member> {}: {value_type}", member.name))
                })
                .unwrap_or(value_type);
            let docs = declaration
                .map(|declaration| declaration.docs.clone())
                .or_else(|| trait_member.map(|member| member.docs.clone()))
                .unwrap_or_default();
            self.syntax_with_docs(expression.syntax(), signature, docs);
        }
        match expression {
            Expression::Function(function) => {
                self.pattern(&function.pattern);
                self.expression(&function.body);
            }
            Expression::Satisfies(satisfies) => {
                self.expression(&satisfies.value);
                self.ty(&satisfies.ty);
            }
            Expression::Match(match_) => {
                self.expression(&match_.subject);
                for arm in &match_.arms {
                    self.pattern(&arm.pattern);
                    self.expression(&arm.body);
                }
            }
            Expression::Loop(loop_) => {
                for item in &loop_.body.items {
                    self.item(item);
                }
            }
            Expression::Resource(resource) => self.ty(&resource.resource),
            Expression::With(with) => {
                self.ty(&with.resource);
                self.expression(&with.value);
                for item in &with.body.items {
                    self.item(item);
                }
            }
            Expression::Block(block) => {
                for item in &block.items {
                    self.item(item);
                }
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    self.expression(&element.value);
                    if let Some(name) = &element.name
                        && let Some(value_type) =
                            self.typed.type_of_expression(element.value.syntax().id)
                    {
                        self.named(&element.syntax, name, self.display_type(value_type));
                    }
                }
            }
            Expression::StringTemplate(value) => {
                for part in &value.parts {
                    if let StringTemplatePart::Interpolation(value) = part {
                        self.expression(&value.expression);
                    }
                }
            }
            Expression::Call(call) => {
                self.expression(&call.callee);
                self.expression(&call.argument);
            }
            Expression::Access(access) => {
                if let Accessor::Method(name) = &access.accessor
                    && let Some(value_type) = self.typed.type_of_expression(access.syntax.id)
                {
                    self.named_last(&access.syntax, name, self.display_type(value_type));
                } else if !self
                    .typed
                    .resolved()
                    .trait_methods_for_expression(access.syntax.id)
                    .is_empty()
                {
                    // A qualified trait member access, e.g.
                    // `ToString.to_string`: the receiver never gets a
                    // checked type of its own, so hover it as the trait
                    // itself instead of falling through to nothing.
                    if let Expression::Name(trait_name) = access.value.as_ref()
                        && let Some(trait_id) =
                            self.typed.resolved().trait_for(trait_name.syntax.id)
                    {
                        self.trait_name_hover(&trait_name.syntax, &trait_name.name, trait_id);
                    } else {
                        self.expression(&access.value);
                    }
                } else if self.typed.resolved().symbol_for(access.syntax.id).is_some() {
                    // A resolved qualified path (namespace member or type
                    // companion member): the receiver never gets a checked
                    // type of its own (typechecking returns early once the
                    // whole chain resolves), so give it a dedicated hover
                    // entry instead of falling through to the generic
                    // type-of-expression path above, which only covers the
                    // outermost node.
                    self.qualified_receiver(&access.value);
                } else {
                    self.expression(&access.value);
                }
            }
            Expression::Index(index) => {
                self.expression(&index.value);
                self.expression(&index.index);
            }
            Expression::Logical(logical) => {
                self.expression(&logical.left);
                self.expression(&logical.right);
                self.ty(&logical.bool_type);
            }
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
            Expression::Quote(quote) => match &quote.template {
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
        if let Some(info) = self
            .typed
            .resolved()
            .compile_time_binding_for(pattern.syntax().id)
        {
            let signature = compile_time_signature(info);
            match pattern {
                Pattern::Binding(binding) => self.named(&binding.syntax, &binding.name, signature),
                Pattern::Splice(splice) => self.named(&splice.syntax, &splice.name, signature),
                _ => {}
            }
        }
        if let Some(value_type) = self.typed.type_of_pattern(pattern.syntax().id) {
            let value_type = self.display_type(value_type);
            self.syntax(pattern.syntax(), value_type.clone());
            if let Pattern::Binding(binding) = pattern {
                let signature = self
                    .typed
                    .symbol_for(binding.syntax.id)
                    .and_then(|symbol| self.declarations.get(&symbol))
                    .map(|declaration| declaration.signature(&value_type))
                    .unwrap_or_else(|| format!("{}: {value_type}", binding.name));
                self.named(&binding.syntax, &binding.name, signature);
            }
        }
        match pattern {
            Pattern::At(at) => {
                self.pattern(&Pattern::Binding(at.binding.as_ref().clone()));
                self.pattern(&at.pattern);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.pattern(element);
                }
            }
            Pattern::Nominal(nominal) => {
                if let Some(id) = self.typed.resolved().type_for_pattern(nominal.syntax.id)
                    && let Some(signature) = self.type_signature(id, nominal.syntax.id)
                {
                    let docs = self
                        .typed
                        .resolved()
                        .type_declarations()
                        .get(&id)
                        .map(|declaration| declaration.docs.clone())
                        .unwrap_or_default();
                    self.named_with_docs(&nominal.syntax, &nominal.name, signature, docs);
                }
                self.pattern(&nominal.argument);
            }
            Pattern::Binding(binding) => self.ty(&binding.ty),
            Pattern::Wildcard(wildcard) => self.ty(&wildcard.ty),
            Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }

    fn type_parameter(&mut self, parameter: &TypeParameterPattern) {
        match parameter {
            TypeParameterPattern::Binding(binding) => self.named(
                &binding.syntax,
                &binding.name,
                format!("<type parameter> {}", binding.name),
            ),
            TypeParameterPattern::Effect(binding) => self.named(
                &binding.syntax,
                &binding.name,
                format!("<effect parameter> {}", binding.name),
            ),
            TypeParameterPattern::Product(product) => {
                for element in &product.elements {
                    self.type_parameter(element);
                }
            }
            TypeParameterPattern::Splice(_) => {}
        }
    }

    fn trait_bound(&mut self, bound: &TraitBound) {
        if let Some(id) = self.typed.resolved().trait_for(bound.syntax.id) {
            self.trait_name_hover(&bound.syntax, &bound.trait_name.name, id);
        }
        for argument in &bound.arguments {
            self.ty(argument);
        }
    }

    fn trait_name_hover(&mut self, syntax: &Syntax, name: &str, trait_id: TraitId) {
        if let Some(resolved) = self.typed.resolved().traits().get(&trait_id) {
            let declaration = &resolved.declaration;
            let (parameters, where_clause) = self.juxtaposed_generic_suffix(
                &declaration.type_parameters,
                &declaration.prerequisites,
                &declaration.subtype_bounds,
                &declaration.functional_dependencies,
            );
            self.named_with_docs(
                syntax,
                name,
                format!("trait {}{parameters}{where_clause}", declaration.name),
                declaration.docs.clone(),
            );
        }
    }

    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Named(named) => {
                if let Some(id) = self.typed.resolved().type_for(named.syntax.id)
                    && let Some(signature) = self.type_signature(id, named.syntax.id)
                {
                    let docs = self
                        .typed
                        .resolved()
                        .type_declarations()
                        .get(&id)
                        .map(|declaration| declaration.docs.clone())
                        .unwrap_or_default();
                    self.named_with_docs(&named.syntax, &named.name, signature, docs);
                } else if self
                    .typed
                    .resolved()
                    .type_parameter_for(named.syntax.id)
                    .is_some()
                {
                    self.named(
                        &named.syntax,
                        &named.name,
                        format!("<type parameter> {}", named.name),
                    );
                }
            }
            Type::Product(product) => {
                for element in &product.elements {
                    self.ty(&element.ty);
                }
            }
            Type::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.ty(alternative);
                }
            }
            Type::Function(function) => {
                self.ty(&function.parameter);
                for resource in &function.effects.resources {
                    self.ty(&resource.value_type);
                }
                self.ty(&function.result);
            }
            Type::Application(application) => {
                self.ty(&application.callee);
                self.ty(&application.argument);
            }
            Type::Repeated(repeated) => {
                self.ty(&repeated.element);
                if let Some(count) = &repeated.count { self.ty(count); }
            }
            Type::Inferred(_) | Type::NumberLiteral(_) | Type::StringLiteral(_) | Type::Splice(_) => {}
        }
    }

    fn syntax(&mut self, syntax: &Syntax, signature: String) {
        self.syntax_with_docs(syntax, signature, Vec::new());
    }

    fn syntax_with_docs(&mut self, syntax: &Syntax, signature: String, documentation: Vec<String>) {
        if let Some(range) = crate::staple_lsp::source_projection::syntax_range(syntax, self.path) {
            self.entries.push(HoverEntry {
                range,
                signature,
                documentation,
            });
        }
    }

    fn named(&mut self, syntax: &Syntax, name: &str, signature: String) {
        self.named_with_docs(syntax, name, signature, Vec::new());
    }

    fn named_with_docs(
        &mut self,
        syntax: &Syntax,
        name: &str,
        signature: String,
        documentation: Vec<String>,
    ) {
        if let Some(range) =
            crate::staple_lsp::source_projection::named_range(syntax, name, false, self.path)
        {
            self.entries.push(HoverEntry {
                range,
                signature,
                documentation,
            });
        }
    }

    fn named_last(&mut self, syntax: &Syntax, name: &str, signature: String) {
        self.named_last_with_docs(syntax, name, signature, Vec::new());
    }

    fn named_last_with_docs(
        &mut self,
        syntax: &Syntax,
        name: &str,
        signature: String,
        documentation: Vec<String>,
    ) {
        if let Some(range) =
            crate::staple_lsp::source_projection::named_range(syntax, name, true, self.path)
        {
            self.entries.push(HoverEntry {
                range,
                signature,
                documentation,
            });
        }
    }
}

fn compile_time_signature(info: &CompileTimeBindingInfo) -> String {
    if info.kind == CompileTimeBindingKind::Builtin {
        return info
            .type_display
            .clone()
            .unwrap_or_else(|| info.name.clone());
    }
    let name = info.declaration_prefix.as_ref().map_or_else(
        || info.name.clone(),
        |prefix| format!("{prefix} {}", info.name),
    );
    match &info.type_display {
        Some(ty) => format!("{name}: {ty}"),
        None => name,
    }
}

fn macro_signature(info: &ResolvedMacro) -> String {
    format!(
        "macro {}{}: {}",
        if info.modifier { "@" } else { "" },
        info.name,
        info.signature
    )
}

impl Declaration {
    fn signature(&self, value_type: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix} {}: {value_type}", self.name),
            None => format!("{}: {value_type}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_hover_uses_configured_name_and_root_docs() {
        let root =
            std::env::temp_dir().join(format!("staple-hover-package-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root_path = root.join("root.sta");
        std::fs::write(
            &root_path,
            "@doc(\"Root package documentation.\")\npub mod\npub let answer = 42\n",
        )
        .unwrap();
        let source = "use package.answer\nanswer\n";
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_module_root(&root)
            .with_package_root(&root_path)
            .with_package_name("example")
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "package"
                && entry.signature == "package example"
                && entry.documentation == ["Root package documentation."]
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qualified_access_segments_hover_intermediate_package_modules() {
        let root = std::env::temp_dir().join(format!(
            "staple-hover-qualified-segments-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("outer")).unwrap();
        std::fs::write(root.join("outer.sta"), "///Outer module.\npub mod\n").unwrap();
        std::fs::write(
            root.join("outer/inner.sta"),
            "///Inner module.\npub mod\npub let answer = 42\n",
        )
        .unwrap();
        let source = "let value: I32 = package.outer.inner.answer\nvalue\n";
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_module_root(&root)
            .with_package_name("example")
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "package" && entry.signature == "package example"
            }),
            "missing package root hover: {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "outer"
                    && entry.signature == "mod outer"
                    && entry.documentation == ["Outer module."]
            }),
            "missing outer segment hover: {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "inner"
                    && entry.signature == "mod inner"
                    && entry.documentation == ["Inner module."]
            }),
            "missing inner segment hover: {entries:?}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    use std::path::PathBuf;

    #[test]
    fn indexes_inferred_declaration_and_reference_types() {
        let source = "let answer = 42\nanswer\n";
        let path = std::env::temp_dir().join("staple-hover-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);
        let references = entries
            .iter()
            .filter(|entry| &source[entry.range.clone()] == "answer")
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 2, "entries: {entries:?}");
        assert!(
            references
                .iter()
                .all(|entry| entry.signature == "let answer: I32")
        );
    }

    #[test]
    fn signal_bindings_show_let_signal_prefix_at_declaration_and_references() {
        let source = "let signal count = 0\ncount = 1\n";
        let path = std::env::temp_dir().join("staple-hover-signal-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);
        let references = entries
            .iter()
            .filter(|entry| &source[entry.range.clone()] == "count")
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 2, "entries: {entries:?}");
        assert!(
            references
                .iter()
                .all(|entry| entry.signature == "let signal count: I32"),
            "entries: {references:?}"
        );
    }

    #[test]
    fn qualified_companion_access_hovers_the_owning_type() {
        let source = concat!(
            "///A boxed integer.\n",
            "type Box = I32\n",
            "companion Box {\n",
            "    pub def create = () => 1\n",
            "}\n",
            "let value = Box.create\n",
        );
        let path = std::env::temp_dir().join("staple-hover-companion-access.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let use_site = source.rfind("Box.create").unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.range.start == use_site && &source[entry.range.clone()] == "Box")
            .unwrap_or_else(|| {
                panic!("no hover entry for the qualified `Box` reference: {entries:?}")
            });
        assert_eq!(entry.signature, "type Box = I32");
        assert_eq!(entry.documentation, vec!["A boxed integer.".to_owned()]);
    }

    #[test]
    fn companion_header_hovers_the_type_declaration() {
        let source = concat!(
            "///A boxed integer.\n",
            "type Box = I32\n",
            "companion Box {\n",
            "    pub def create = () => 1\n",
            "}\n",
        );
        let path = std::env::temp_dir().join("staple-hover-companion-header.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let header = source.find("companion Box").unwrap() + "companion ".len();
        let entry = entries
            .iter()
            .find(|entry| entry.range.start == header && &source[entry.range.clone()] == "Box")
            .unwrap_or_else(|| {
                panic!("no hover entry for the `companion Box` header: {entries:?}")
            });
        assert_eq!(entry.signature, "type Box = I32");
        assert_eq!(entry.documentation, vec!["A boxed integer.".to_owned()]);
    }

    #[test]
    fn use_glob_path_segment_hovers_the_companion_type() {
        let source = concat!(
            "///A boxed integer.\n",
            "type Box = I32\n",
            "companion Box {\n",
            "    pub type Inner\n",
            "}\n",
            "use Box.*\n",
        );
        let path = std::env::temp_dir().join("staple-hover-use-companion-segment.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let segment = source.find("use Box.*").unwrap() + "use ".len();
        let entry = entries
            .iter()
            .find(|entry| entry.range.start == segment && &source[entry.range.clone()] == "Box")
            .unwrap_or_else(|| panic!("no hover entry for `Box` in `use Box.*`: {entries:?}"));
        assert_eq!(entry.signature, "type Box = I32");
        assert_eq!(entry.documentation, vec!["A boxed integer.".to_owned()]);
    }

    #[test]
    fn use_glob_of_a_typegroup_hovers_the_generated_alias() {
        let source = concat!(
            "typegroup Switch {\n",
            "    Enabled,\n",
            "    Disabled,\n",
            "}\n",
            "use Switch.*\n",
        );
        let path = std::env::temp_dir().join("staple-hover-use-typegroup-segment.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let segment = source.find("use Switch.*").unwrap() + "use ".len();
        let entry = entries
            .iter()
            .find(|entry| entry.range.start == segment && &source[entry.range.clone()] == "Switch")
            .unwrap_or_else(|| {
                panic!("no hover entry for `Switch` in `use Switch.*`: {entries:?}")
            });
        assert!(
            entry.signature.starts_with("type alias Switch = "),
            "unexpected `use Switch.*` segment signature: {entry:?}"
        );
    }

    #[test]
    fn hand_written_companion_header_alongside_typegroup_hovers_the_alias() {
        let source = concat!(
            "typegroup Switch {\n",
            "    Enabled,\n",
            "    Disabled,\n",
            "}\n",
            "companion Switch {}\n",
        );
        let path = std::env::temp_dir().join("staple-hover-typegroup-companion-header.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let header = source.find("companion Switch").unwrap() + "companion ".len();
        let entry = entries
            .iter()
            .find(|entry| entry.range.start == header && &source[entry.range.clone()] == "Switch")
            .unwrap_or_else(|| {
                panic!("no hover entry for the `companion Switch` header: {entries:?}")
            });
        assert!(
            entry.signature.starts_with("type alias Switch = "),
            "unexpected companion header signature: {entry:?}"
        );
    }

    #[test]
    fn qualified_trait_access_hovers_the_trait() {
        let source = concat!(
            "///Converts a value to its string representation.\n",
            "trait ToString T { to_string: T -> String }\n",
            "impl ToString I32 { def to_string = value => \"\" }\n",
            "def f: I32 -> String = ToString.to_string\n",
        );
        let path = std::env::temp_dir().join("staple-hover-trait-access.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let use_site = source.rfind("ToString.to_string").unwrap();
        let entry = entries
            .iter()
            .find(|entry| {
                entry.range.start == use_site && &source[entry.range.clone()] == "ToString"
            })
            .unwrap_or_else(|| {
                panic!("no hover entry for the qualified `ToString` reference: {entries:?}")
            });
        assert_eq!(entry.signature, "trait ToString T");
        assert_eq!(
            entry.documentation,
            vec!["Converts a value to its string representation.".to_owned()]
        );
    }

    #[test]
    fn generic_def_and_type_signatures_use_the_new_syntax() {
        let source =
            "pub(repr) type Box T = (value: T)\ndef unbox: <T> Box T -> T = Box value => value\n";
        let path = std::env::temp_dir().join("staple-hover-generic-signatures.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        for (name, signature) in [
            ("Box", "type Box T = (value: T)"),
            ("unbox", "def unbox: <T> Box T -> T"),
        ] {
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == name && entry.signature == signature
                }),
                "missing {name}: {signature} in {entries:?}"
            );
        }
    }

    #[test]
    fn reaction_effects_propagate_through_a_wrapping_function() {
        let source = concat!(
            "use std.io.println\n",
            "let signal count = 0\n",
            "let doubled = count * 2\n",
            "with Reactive = reactive_scope () {\n",
            "    def test = () => {\n",
            "        reaction {\n",
            "            println \"count: $count, doubled: $doubled\"\n",
            "        }\n",
            "    }\n",
            "    test()\n",
            "    count = 1\n",
            "    count = 2\n",
            "}\n",
        );
        let path = std::env::temp_dir().join("staple-hover-reaction-in-wrapping-function.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        for (name, signature) in [
            ("test", "def test: () ->{state.read, IO, Reactive} ()"),
            (
                "reaction",
                "def reaction: (() ->{state.read, IO} ()) ->{state.read, IO, Reactive} ()",
            ),
        ] {
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == name && entry.signature == signature
                }),
                "missing {name}: {signature} in {entries:?}"
            );
        }
    }

    #[test]
    fn indexes_at_pattern_aliases_and_nested_bindings() {
        let source =
            "def sum = pair@(left: I32, right: I32) => pair.0 + left + right\nsum (20, 22)\n";
        let path = std::env::temp_dir().join("staple-hover-at-patterns.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        for (name, signature) in [
            ("pair", "pair: (left: I32, right: I32)"),
            ("left", "left: I32"),
            ("right", "right: I32"),
        ] {
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == name && entry.signature == signature
                }),
                "missing {name}: {signature} in {entries:?}"
            );
        }
    }

    #[test]
    fn includes_explicit_and_triple_slash_docs_on_declarations_and_references() {
        let source = concat!(
            "@doc(\"Line 1\")\n",
            "///Line 2\n",
            "pub type alias MyType = I32\n",
            "/// Value docs\n",
            "def value: MyType = 1\n",
            "value\n",
        );
        let path = std::env::temp_dir().join("staple-hover-docs.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let type_entries = entries
            .iter()
            .filter(|entry| &source[entry.range.clone()] == "MyType")
            .collect::<Vec<_>>();
        assert!(type_entries.len() >= 2, "{type_entries:?}");
        assert!(
            type_entries
                .iter()
                .all(|entry| entry.documentation == ["Line 1", "Line 2"])
        );

        let value_entries = entries
            .iter()
            .filter(|entry| &source[entry.range.clone()] == "value")
            .collect::<Vec<_>>();
        assert!(value_entries.len() >= 2, "{value_entries:?}");
        assert!(
            value_entries
                .iter()
                .all(|entry| entry.documentation == [" Value docs"])
        );
    }

    #[test]
    fn displays_inferred_resource_contracts() {
        let source = "type Clock = I32\ndef read = () => resource Clock\n";
        let path = std::env::temp_dir().join("staple-hover-resources.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "read"
                && entry.signature == "def read: () ->{Clock} Clock"
        }));
    }

    #[test]
    fn indexes_macro_declarations_and_selected_invocations() {
        let root = std::env::temp_dir().join(format!(
            "staple-hover-macro-invocations-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            "pub mod\nuse std.syntax.(parse_quote, Expr)\npub macro imported: Expr -> Expr = value: Expr => parse_quote { $value }\n",
        )
        .unwrap();
        let source = concat!(
            "use std.syntax.(parse_quote, Expr, CallExpr, Item)\n",
            "use dependency\n",
            "macro choose: Expr -> Expr = _: Expr => parse_quote { 1 }\n",
            "macro choose: CallExpr -> Expr = _: CallExpr => parse_quote { 2 }\n",
            "macro inferred = value => parse_quote { $value }\n",
            "macro satisfied = expr: Expr => parse_quote { $expr } satisfies Expr\n",
            "macro @identity: Item -> Item = item: Item => item\n",
            "let selected = choose (discarded 0)\n",
            "let inferred_value = inferred 4\n",
            "let satisfied_value = satisfied 5\n",
            "@doc(\"A decorated value.\")\n",
            "@identity let decorated = dependency.imported 3\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);
        let signatures = entries
            .iter()
            .map(|entry| (&source[entry.range.clone()], entry.signature.as_str()))
            .collect::<Vec<_>>();

        assert!(signatures.contains(&("choose", "macro choose: Expr -> Expr")));
        assert!(signatures.contains(&("choose", "macro choose: CallExpr -> Expr")));
        assert!(signatures.contains(&("identity", "macro @identity: Item -> Item")));
        assert!(signatures.contains(&("doc", "macro @doc: Parenthesized (Expr) -> Item -> Item")));
        assert!(signatures.contains(&("inferred", "macro inferred: SyntaxNode -> Expr")));
        assert!(signatures.contains(&("satisfied", "macro satisfied: Expr -> Expr")));
        assert!(signatures.contains(&("imported", "macro imported: Expr -> Expr")));
        assert!(
            signatures.contains(&("parse_quote", "macro parse_quote: Braced Syntax -> Syntax"))
        );
        assert!(
            signatures
                .iter()
                .filter(|signature| **signature == ("choose", "macro choose: CallExpr -> Expr"))
                .count()
                >= 2,
            "signatures: {signatures:?}"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_quotation_macro_declarations() {
        let stdlib_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let path = stdlib_root.join("std").join("syntax.sta");
        let source = std::fs::read_to_string(&path).unwrap();
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib_root)
            .load_source_at(&path, &source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(&source).unwrap();
        let entries = entries(&module, &typed);
        let signatures = entries
            .iter()
            .map(|entry| (&source[entry.range.clone()], entry.signature.as_str()))
            .collect::<Vec<_>>();

        assert!(
            signatures
                .iter()
                .filter(|(text, _)| *text == "Braced")
                .count()
                >= 4,
            "signatures: {signatures:?}"
        );
        assert!(
            signatures
                .iter()
                .filter(|(text, _)| *text == "Syntax")
                .count()
                >= 6,
            "signatures: {signatures:?}"
        );
    }

    #[test]
    fn displays_declared_types_for_macro_helper_parameters() {
        let stdlib_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let path = stdlib_root.join("std").join("typegroup.sta");
        let source = std::fs::read_to_string(&path).unwrap();
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib_root)
            .load_source_at(&path, &source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(&source).unwrap();
        let entries = entries(&module, &typed);

        for expected in [
            ("visibility", "visibility: Visibility"),
            (
                "entries",
                "entries: Sequence (Sequence Modifier, Ident String, Optional Type)",
            ),
        ] {
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == expected.0 && entry.signature == expected.1
                }),
                "missing {:?} in {entries:?}",
                expected
            );
        }
    }

    #[test]
    fn formats_def_and_trait_member_declarations() {
        let source = concat!(
            "trait Identity T { identity: T -> T }\n",
            "impl Identity I32 { def identity = value => value }\n",
            "def apply = () => identity 1\n",
            "apply\n",
        );
        let path = std::env::temp_dir().join("staple-hover-declarations-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "apply" && entry.signature == "def apply: () -> I32"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "identity"
                && entry.signature.starts_with("<trait member> identity: ")
        }));
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "identity"
                    && entry.signature == "def identity: I32 -> I32"
            }),
            "entries: {entries:?}"
        );
    }

    #[test]
    fn formats_extern_binding_declarations() {
        let source = concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { printf: (CPointer CChar, ...) -> I32 }\n",
        );
        let path = std::env::temp_dir().join("staple-hover-extern-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "printf"
                    && entry.signature.starts_with("<extern> printf: ")
            }),
            "entries: {entries:?}"
        );
    }

    #[test]
    fn function_type_hover_shows_mut_and_move_markers_together() {
        let source = concat!(
            "def mutate_and_consume: (mut I32, move I32) -> () = (mut first, move second) => ()\n",
        );
        let path = std::env::temp_dir().join("staple-hover-mut-move-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "mutate_and_consume"
                    && entry.signature == "def mutate_and_consume: (mut I32, move I32) -> ()"
            }),
            "entries: {entries:?}"
        );
    }

    #[test]
    fn mutable_parameter_hover_shows_mut_at_every_usage() {
        let source = "def foo = (mut x: I32, mut y: Bool) => {\n    x\n}\n";
        let path = std::env::temp_dir().join("staple-hover-mutable-parameter-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        let declaration_start = source.find("x:").unwrap();
        let usage_start = source.rfind('x').unwrap();
        for start in [declaration_start, usage_start] {
            assert!(
                entries
                    .iter()
                    .any(|entry| { entry.range.start == start && entry.signature == "mut x: I32" }),
                "no `mut x: I32` hover at {start}: {entries:?}"
            );
        }
    }

    #[test]
    fn whole_mut_product_pattern_shows_mut_for_every_destructured_element() {
        let source = "def foo = mut (x: I32, y: I32) => {\n    x = 32\n    y = 64\n}\n";
        let path = std::env::temp_dir().join("staple-hover-whole-mut-product-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        for (declaration_needle, usage_needle, expected) in [
            ("x: I32", "x = 32", "mut x: I32"),
            ("y: I32", "y = 64", "mut y: I32"),
        ] {
            let declaration_start = source.find(declaration_needle).unwrap();
            let usage_start = source.find(usage_needle).unwrap();
            for start in [declaration_start, usage_start] {
                assert!(
                    entries
                        .iter()
                        .any(|entry| entry.range.start == start && entry.signature == expected),
                    "no `{expected}` hover at {start}: {entries:?}"
                );
            }
        }
    }

    #[test]
    fn function_type_hover_parenthesizes_a_function_parameter() {
        let source = concat!("def foo: (() -> I32) -> I32 = x => x ()\n",);
        let path = std::env::temp_dir().join("staple-hover-nested-function-param-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "foo"
                    && entry.signature == "def foo: (() -> I32) -> I32"
            }),
            "entries: {entries:?}"
        );
    }

    #[test]
    fn indexes_generic_trait_implementation_generics() {
        let source = concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: move T -> T }\n",
            "impl Bound I32 { def check = value => True }\n",
            "impl <T where Bound T> Target T { def act = move value => value }\n",
        );
        let path = std::env::temp_dir().join("staple-hover-generic-impl-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "T" && entry.signature == "<type parameter> T"
            }),
            "entries: {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                &source[entry.range.clone()] == "act" && entry.signature.starts_with("def act: ")
            }),
            "entries: {entries:?}"
        );
    }

    #[test]
    fn formats_imported_value_declarations() {
        let root =
            std::env::temp_dir().join(format!("staple-hover-import-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            "pub mod\npub def imported = () => 1\npub let imported_value = 2\n",
        )
        .unwrap();
        let source = concat!(
            "use dependency.(imported, imported_value)\n",
            "use dependency\n",
            "imported ()\n",
            "imported_value\n",
            "dependency.imported ()\n",
            "dependency.imported_value\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "imported"
                && entry.signature == "def imported: () -> I32"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "imported_value"
                && entry.signature == "let imported_value: I32"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "dependency.imported"
                && entry.signature == "def imported: () -> I32"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "dependency.imported_value"
                && entry.signature == "let imported_value: I32"
        }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn describes_imported_items_in_use_declarations() {
        let root =
            std::env::temp_dir().join(format!("staple-hover-use-items-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            concat!(
                "@doc(\"Dependency module.\")\n",
                "pub mod\n",
                "/// A value.\n",
                "pub let value = 1\n",
                "/// A callable.\n",
                "pub def callable = () => 1\n",
                "/// A number alias.\n",
                "pub type alias Number = I32\n",
                "/// A printable trait.\n",
                "pub trait Printable T {}\n",
                "/// An identity macro.\n",
                "pub macro identity = value => parse_quote { $value }\n",
            ),
        )
        .unwrap();
        let source = concat!(
            "use dependency.(value, Number, Printable, identity)\n",
            "use dependency.callable as invoke\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed)
            .into_iter()
            .map(|entry| (&source[entry.range], entry.signature, entry.documentation))
            .collect::<Vec<_>>();

        for expected in [
            ("dependency", "mod dependency", "Dependency module."),
            ("value", "let value: I32", " A value."),
            ("Number", "type alias Number = I32", " A number alias."),
            ("Printable", "trait Printable T", " A printable trait."),
            (
                "identity",
                "macro identity: SyntaxNode -> Expr",
                " An identity macro.",
            ),
            ("callable", "def callable: () -> I32", " A callable."),
            ("invoke", "def callable: () -> I32", " A callable."),
        ] {
            assert!(
                entries.iter().any(|entry| entry.0 == expected.0
                    && entry.1 == expected.1
                    && entry.2 == [expected.2]),
                "missing {expected:?} in {entries:?}"
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn describes_module_segments_in_dotted_use_declarations() {
        let source = "use std.io.println\n";
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, &root)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        for (name, signature) in [("std", "package std"), ("io", "mod io")] {
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == name && entry.signature == signature
                }),
                "missing module hover for {name}: {entries:?}"
            );
        }
        assert!(!entries.iter().any(|entry| {
            &source[entry.range.clone()] == "println" && entry.signature == "mod println"
        }));
    }

    #[test]
    fn only_describes_existing_file_module_segments_and_includes_their_docs() {
        let root = std::env::temp_dir().join(format!(
            "staple-hover-module-segments-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::write(
            root.join("alpha.sta"),
            "@doc(\"Alpha docs.\")\npub mod\npub let root_value = 1\n",
        )
        .unwrap();
        std::fs::write(
            root.join("alpha/beta.sta"),
            "@doc(\"Beta docs.\")\npub mod\npub let value = 1\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("alpha/missing")).unwrap();
        std::fs::write(
            root.join("alpha/missing/leaf.sta"),
            "pub mod\npub let leaf_value = 1\n",
        )
        .unwrap();
        let source = concat!(
            "use alpha\n",
            "use alpha.beta.value\n",
            "use alpha.missing.leaf.leaf_value\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        for (name, signature, docs) in [
            ("alpha", "mod alpha", "Alpha docs."),
            ("beta", "mod beta", "Beta docs."),
        ] {
            assert!(
                entries.iter().any(|entry| {
                    &source[entry.range.clone()] == name
                        && entry.signature == signature
                        && entry.documentation == [docs]
                }),
                "missing documented module hover for {name}: {entries:?}"
            );
        }
        assert!(!entries.iter().any(|entry| {
            &source[entry.range.clone()] == "value" && entry.signature == "mod value"
        }));
        assert!(!entries.iter().any(|entry| {
            &source[entry.range.clone()] == "missing" && entry.signature == "mod missing"
        }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inferred_imported_types_do_not_expose_internal_module_ids() {
        let root = std::env::temp_dir().join(format!(
            "staple-hover-imported-type-name-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("geometry.sta"),
            concat!(
                "pub mod\n",
                "pub(repr) type Point = (x: I32, y: I32)\n",
                "pub def origin = () => Point (x: 0, y: 0)\n",
            ),
        )
        .unwrap();
        let source = concat!(
            "use geometry.(Point, origin)\n",
            "let start: Point = origin ()\n",
            "start\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);
        let start_entries = entries
            .iter()
            .filter(|entry| &source[entry.range.clone()] == "start")
            .collect::<Vec<_>>();

        assert_eq!(start_entries.len(), 2, "entries: {entries:?}");
        assert!(
            start_entries
                .iter()
                .all(|entry| entry.signature == "let start: Point"),
            "entries: {entries:?}"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn formats_local_type_declarations_and_references() {
        let source = concat!(
            "type Box T = (value: T)\n",
            "type alias Pair (A, B) = (A, B)\n",
            "def keep: Box I32 -> Box I32 = value => value\n",
            "def pair: Pair (I32, I32) -> Pair (I32, I32) = value => value\n",
        );
        let path = std::env::temp_dir().join("staple-hover-local-types-test.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "Box" && entry.signature == "type Box T = (value: T)"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "Pair"
                && entry.signature == "type alias Pair (A, B) = (A, B)"
        }));
    }

    #[test]
    fn respects_imported_type_representation_visibility() {
        let root = std::env::temp_dir().join(format!(
            "staple-hover-import-types-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("dependency.sta"),
            concat!(
                "pub mod\n",
                "pub type Hidden = I32\n",
                "pub type HiddenGeneric T = T\n",
                "pub(repr) type Visible = I32\n",
                "pub type alias Alias = I32\n",
            ),
        )
        .unwrap();
        let source = concat!(
            "use dependency.(Hidden, HiddenGeneric, Visible, Alias)\n",
            "def hidden: Hidden -> Hidden = value => value\n",
            "def hidden_generic: HiddenGeneric I32 -> HiddenGeneric I32 = value => value\n",
            "def visible: Visible -> Visible = value => value\n",
            "def alias_value: Alias -> Alias = value => value\n",
        );
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(source).unwrap();
        let entries = entries(&module, &typed);

        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "Hidden" && entry.signature == "type Hidden"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "HiddenGeneric"
                && entry.signature == "type HiddenGeneric T"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "Visible" && entry.signature == "type Visible = I32"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "Alias" && entry.signature == "type alias Alias = I32"
        }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn describes_macro_meta_locals_and_helpers() {
        let stdlib = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let path = stdlib.join("std/flow.sta");
        let source = std::fs::read_to_string(&path).unwrap();
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib)
            .load_source_at(&path, &source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(&source).unwrap();
        let entries = entries(&module, &typed);
        let signatures = entries
            .iter()
            .map(|entry| (&source[entry.range.clone()], entry.signature.as_str()))
            .collect::<Vec<_>>();

        assert!(
            signatures
                .iter()
                .any(|entry| entry.0 == "otherwise" && entry.1.starts_with("let otherwise:")),
            "{signatures:?}"
        );
        assert!(
            signatures
                .iter()
                .any(|entry| entry.0 == "body" && entry.1.starts_with("body: Braced")),
            "{signatures:?}"
        );
        assert!(
            signatures
                .iter()
                .any(|entry| entry.0 == "when_clauses" && entry.1.starts_with("def when_clauses:")),
            "{signatures:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                entry.range.start < 801
                    && &source[entry.range.clone()] == "Expr"
                    && entry.signature.starts_with("type alias Expr")
            }),
            "{signatures:?}"
        );
    }

    #[test]
    fn formats_compile_time_lets_and_syntax_constructors_like_source_declarations() {
        let stdlib = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/macros.sta");
        let source = std::fs::read_to_string(&path).unwrap();
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib)
            .load_source_at(&path, &source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(&source).unwrap();
        let entries = entries(&module, &typed);
        let signatures = entries
            .iter()
            .map(|entry| (&source[entry.range.clone()], entry.signature.as_str()))
            .collect::<Vec<_>>();

        assert!(
            signatures.contains(&("original", "let original: CallExpr")),
            "{signatures:?}"
        );
        assert!(
            signatures.contains(&("changed", "let mut changed: CallExpr")),
            "{signatures:?}"
        );
        assert!(
            signatures.contains(&("CallExpr", "(callee: Expr, argument: Expr) -> CallExpr")),
            "{signatures:?}"
        );
        assert!(
            !signatures
                .iter()
                .any(|entry| entry.1.contains("<compile-time")
                    || entry.1.contains("<macro parameter>")
                    || entry.1.contains("<syntax category>"))
        );
    }
}
