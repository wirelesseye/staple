use std::collections::HashMap;
use std::ops::Range;

use stapler::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverEntry {
    pub range: Range<usize>,
    pub signature: String,
}

pub fn entries(module: &Module, typed: &TypedModule) -> Vec<HoverEntry> {
    let mut collector = Collector {
        typed,
        entries: Vec::new(),
        declarations: HashMap::new(),
    };
    for source_module in typed.resolved().program().modules() {
        collector.collect_module_declarations(&source_module.syntax);
    }
    // Also scan the editor-owned syntax, which retains unexpanded source IDs.
    collector.collect_module_declarations(module);
    for item in &module.items {
        collector.item(item);
    }
    collector.entries
}

struct Collector<'a> {
    typed: &'a TypedModule,
    entries: Vec<HoverEntry>,
    declarations: HashMap<SymbolId, Declaration>,
}

#[derive(Clone)]
struct Declaration {
    prefix: Option<&'static str>,
    name: String,
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
                        self.named_last(&modifier.syntax, &modifier.name, macro_signature(info));
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
            statement @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.collect_statement_declarations(statement),
            Item::UseDeclaration(_) | Item::TypeDeclaration(_) => {}
        }
    }

    fn collect_statement_declarations(&mut self, statement: &Item) {
        match statement {
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
            Item::Submodule(submodule) => {
                self.collect_module_declarations(&submodule.module)
            }
            Item::TypeDeclaration(_) => {}
            Item::UseDeclaration(_) => {}
            _ => {}
        }
    }

    fn collect_binding_declaration(&mut self, binding: &Binding) {
        if let Some(symbol) = self.typed.symbol_for(binding.syntax.id) {
            self.declarations.insert(
                symbol,
                Declaration {
                    prefix: Some(binding.keyword()),
                    name: binding.name.clone(),
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
            Expression::Call(call) => {
                self.collect_expression_declarations(&call.callee);
                self.collect_expression_declarations(&call.argument);
            }
            Expression::Access(access) => self.collect_expression_declarations(&access.value),
            Expression::Index(index) => {
                self.collect_expression_declarations(&index.value);
                self.collect_expression_declarations(&index.index);
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
                    let prefix = is_let_context.then(|| {
                        if binding.reassignable {
                            "var"
                        } else {
                            "let"
                        }
                    });
                    self.declarations.insert(
                        symbol,
                        Declaration {
                            prefix,
                            name: binding.name.clone(),
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
            Item::VisibilityMacroInvocation(value) => self.expression(&value.expression),
            Item::VisibilitySplice(value) => self.item(&value.item),
            Item::RepeatedItemSplice(_) => {}
            Item::Submodule(submodule) => {
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
                    self.named(
                        &declaration.syntax,
                        &declaration.name,
                        macro_signature(info),
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
                        self.named(
                            &member.syntax,
                            &member.name,
                            format!("def {}: {value_type}", member.name),
                        );
                    }
                    self.expression(&member.value);
                }
            }
            Item::TraitDeclaration(declaration) => {
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
                    self.named(
                        &member.syntax,
                        &member.name,
                        format!(
                            "<trait member> {}: {}",
                            member.name,
                            member.annotation.syntax().text().trim()
                        ),
                    );
                    self.ty(&member.annotation);
                    if let Some(default) = &member.default {
                        self.expression(default);
                    }
                }
            }
            Item::TypeDeclaration(declaration) => self.type_declaration(declaration),
            statement @ (Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)) => self.statement(statement),
            Item::UseDeclaration(declaration) => self.use_declaration(declaration),
        }
    }

    fn use_declaration(&mut self, declaration: &UseDeclaration) {
        match self.typed.resolved().program().use_kind(declaration) {
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
            .find_map(|definition| self.definition_signature(*definition, declaration.syntax.id));
        if let Some(signature) = signature {
            if last {
                self.named_last(&declaration.syntax, token_name, signature);
            } else {
                self.named(&declaration.syntax, token_name, signature);
            }
        }
    }

    fn definition_signature(
        &self,
        definition: DefinitionId,
        from_syntax: SyntaxId,
    ) -> Option<String> {
        let resolved = self.typed.resolved();
        match definition {
            DefinitionId::Symbol(symbol) => {
                let value_type = self
                    .typed
                    .type_of_symbol(symbol)
                    .map(|ty| self.display_type(ty))?;
                Some(
                    self.declarations
                        .get(&symbol)
                        .map(|declaration| declaration.signature(&value_type))
                        .unwrap_or(value_type),
                )
            }
            DefinitionId::Type(id) => self.type_signature(id, from_syntax),
            DefinitionId::Trait(id) => resolved
                .traits()
                .get(&id)
                .map(|resolved| format!("trait {}", resolved.declaration.name)),
            DefinitionId::TraitMethod(id) => resolved.trait_method(id).map(|member| {
                format!(
                    "<trait member> {}: {}",
                    member.name,
                    member.annotation.syntax().text().trim()
                )
            }),
            DefinitionId::Macro(id) => resolved.macro_for(id).map(macro_signature),
            DefinitionId::CompileTime(syntax) => resolved
                .compile_time_binding_for(syntax)
                .map(compile_time_signature),
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
            self.named(&declaration.syntax, &declaration.name, signature);
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

    fn type_signature(&self, id: TypeId, from_syntax: SyntaxId) -> Option<String> {
        let resolved = self.typed.resolved();
        let declaration = resolved.type_declarations().get(&id)?;
        let from_module = resolved
            .module_for_syntax(from_syntax)
            .unwrap_or_else(|| resolved.program().entry());
        let representation_is_visible = declaration.kind == TypeDeclarationKind::Alias
            || resolved.representation_visible_from(id, from_module);
        if !representation_is_visible && declaration.type_parameters.is_empty() {
            return Some(format!("type {}", declaration.name));
        }
        let representation = if representation_is_visible {
            declaration
                .underlying
                .as_ref()
                .map(|ty| ty.to_string())
                .unwrap_or_else(|| match declaration.kind {
                    TypeDeclarationKind::Opaque => "opaque".to_owned(),
                    TypeDeclarationKind::Singleton => "()".to_owned(),
                    TypeDeclarationKind::Alias | TypeDeclarationKind::Distinct => "...".to_owned(),
                })
        } else {
            "...".to_owned()
        };
        let alias = if declaration.kind == TypeDeclarationKind::Alias {
            " alias"
        } else {
            ""
        };
        let parameters = declaration
            .type_parameters
            .iter()
            .map(|parameter| parameter.syntax().text().trim().to_owned())
            .collect::<Vec<_>>();
        let parameters = if parameters.is_empty() {
            String::new()
        } else {
            format!("{} => ", parameters.join(" => "))
        };
        Some(format!(
            "type{alias} {} = {parameters}{representation}",
            declaration.name
        ))
    }

    fn statement(&mut self, statement: &Item) {
        match statement {
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
            self.named(
                &binding.syntax,
                &binding.name,
                compile_time_signature(info),
            );
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
            let prefix = binding.keyword();
            self.named(
                &binding.syntax,
                &binding.name,
                format!("{prefix} {}: {value_type}", binding.name),
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
                Expression::Name(name) => {
                    self.named(&name.syntax, &name.name, macro_signature(info))
                }
                Expression::Access(access) => {
                    if let Accessor::Name(name) = &access.accessor {
                        self.named_last(&access.syntax, name, macro_signature(info));
                    }
                }
                _ => {}
            }
        }
        if let Some(info) = self.typed.resolved().compile_time_binding_for(expression.syntax().id) {
            let signature = compile_time_signature(info);
            match expression {
                Expression::Name(name) => self.named(&name.syntax, &name.name, signature),
                Expression::Splice(splice) => self.named(&splice.syntax, &splice.name, signature),
                _ => {}
            }
        }
        if let Some(value_type) = self.typed.type_of_expression(expression.syntax().id) {
            let value_type = self.display_type(value_type);
            let signature = self
                .typed
                .symbol_for(expression.syntax().id)
                .and_then(|symbol| self.declarations.get(&symbol))
                .map(|declaration| declaration.signature(&value_type))
                .or_else(|| {
                    self.typed
                        .resolved()
                        .trait_methods_for_expression(expression.syntax().id)
                        .first()
                        .and_then(|method| self.typed.resolved().trait_method(*method))
                        .map(|member| format!("<trait member> {}: {value_type}", member.name))
                })
                .unwrap_or(value_type);
            self.syntax(expression.syntax(), signature);
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
            Expression::Call(call) => {
                self.expression(&call.callee);
                self.expression(&call.argument);
            }
            Expression::Access(access) => self.expression(&access.value),
            Expression::Index(index) => {
                self.expression(&index.value);
                self.expression(&index.index);
            }
            Expression::SyntaxArgument(_) | Expression::VisibilityArgument(_) => {}
            Expression::Quote(quote) => {
                let signature = match quote.kind {
                    QuoteKind::Quote => "macro quote: Braced Syntax -> Syntax",
                    QuoteKind::ParseQuote => "macro parse_quote: T => ParseQuoteResult T => Braced Syntax -> T",
                };
                self.named(&quote.syntax, quote.kind.name(), signature.to_owned());
                match &quote.template {
                    QuoteTemplate::Expression(expression) => self.expression(expression),
                    QuoteTemplate::Item(item) => self.item(item),
                    QuoteTemplate::Items(items) => items.iter().for_each(|item| self.item(item)),
                    QuoteTemplate::Raw => {}
                }
            }
            Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        if let Some(info) = self.typed.resolved().compile_time_binding_for(pattern.syntax().id) {
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
                    self.named(&nominal.syntax, &nominal.name, signature);
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
            TypeParameterPattern::Product(product) => {
                for element in &product.elements {
                    self.type_parameter(element);
                }
            }
            TypeParameterPattern::Splice(_) => {}
        }
    }

    fn trait_bound(&mut self, bound: &TraitBound) {
        for argument in &bound.arguments {
            self.ty(argument);
        }
    }

    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Named(named) => {
                if let Some(id) = self.typed.resolved().type_for(named.syntax.id)
                    && let Some(signature) = self.type_signature(id, named.syntax.id)
                {
                    self.named(&named.syntax, &named.name, signature);
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
                for resource in &function.resources.resources {
                    self.ty(resource);
                }
                self.ty(&function.result);
            }
            Type::Application(application) => {
                self.ty(&application.callee);
                self.ty(&application.argument);
            }
            Type::Repeated(repeated) => self.ty(&repeated.element),
            Type::Inferred(_) | Type::StringLiteral(_) | Type::Splice(_) => {}
        }
    }

    fn syntax(&mut self, syntax: &Syntax, signature: String) {
        if let (Some(first), Some(last)) = (
            syntax.tokens().iter().find(|token| !token.kind.is_trivia()),
            syntax
                .tokens()
                .iter()
                .rev()
                .find(|token| !token.kind.is_trivia()),
        ) {
            self.entries.push(HoverEntry {
                range: first.span.start..last.span.end,
                signature,
            });
        }
    }

    fn named(&mut self, syntax: &Syntax, name: &str, signature: String) {
        if let Some(token) = syntax.tokens().iter().find(|token| token.text == name) {
            self.entries.push(HoverEntry {
                range: token.span.clone(),
                signature,
            });
        }
    }

    fn named_last(&mut self, syntax: &Syntax, name: &str, signature: String) {
        if let Some(token) = syntax
            .tokens()
            .iter()
            .rev()
            .find(|token| token.text == name)
        {
            self.entries.push(HoverEntry {
                range: token.span.clone(),
                signature,
            });
        }
    }
}

fn compile_time_signature(info: &CompileTimeBindingInfo) -> String {
    if info.kind == CompileTimeBindingKind::Builtin {
        return info.type_display.clone().unwrap_or_else(|| info.name.clone());
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
        match self.prefix {
            Some(prefix) => format!("{prefix} {}: {value_type}", self.name),
            None => format!("{}: {value_type}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "use std.syntax.(parse_quote, Expr)\npub macro imported: Expr -> Expr = value: Expr => parse_quote { $value }\n",
        )
        .unwrap();
        let source = concat!(
            "use std.syntax.(parse_quote, Expr, CallExpr, Item)\n",
            "use dependency\n",
            "macro choose: Expr -> Expr = _: Expr => parse_quote { 1 }\n",
            "macro choose: CallExpr -> Expr = _: CallExpr => parse_quote { 2 }\n",
            "macro inferred = value => parse_quote { $value }\n",
            "macro @identity: Item -> Item = item: Item => item\n",
            "let selected = choose (discarded 0)\n",
            "let inferred_value = inferred 4\n",
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
        assert!(signatures.contains(&("inferred", "macro inferred: SyntaxNode -> Expr")));
        assert!(signatures.contains(&("imported", "macro imported: Expr -> Expr")));
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
    fn indexes_macro_declaration_generics() {
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
                .filter(|signature| **signature == ("T", "<type parameter> T"))
                .count()
                >= 3,
            "signatures: {signatures:?}"
        );
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
    fn indexes_generic_trait_implementation_generics() {
        let source = concat!(
            "trait Bound T { check: T -> Bool }\n",
            "trait Target T { act: T -> T }\n",
            "impl Bound I32 { def check = value => True }\n",
            "impl <T where Bound T> Target T { def act = value => value }\n",
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
            "pub def imported = () => 1\npub let imported_value = 2\n",
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
                "pub let value = 1\n",
                "pub def callable = () => 1\n",
                "pub type alias Number = I32\n",
                "pub trait Printable T {}\n",
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
            .map(|entry| (&source[entry.range], entry.signature))
            .collect::<Vec<_>>();

        for expected in [
            ("value", "let value: I32"),
            ("Number", "type alias Number = I32"),
            ("Printable", "trait Printable"),
            ("identity", "macro identity: SyntaxNode -> Expr"),
            ("callable", "def callable: () -> I32"),
            ("invoke", "def callable: () -> I32"),
        ] {
            assert!(
                entries
                    .iter()
                    .any(|entry| entry.0 == expected.0 && entry.1 == expected.1),
                "missing {expected:?} in {entries:?}"
            );
        }

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
            &source[entry.range.clone()] == "Box" && entry.signature == "type Box = T => (value: T)"
        }));
        assert!(entries.iter().any(|entry| {
            &source[entry.range.clone()] == "Pair"
                && entry.signature == "type alias Pair = (A, B) => (A, B)"
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
                && entry.signature == "type HiddenGeneric = T => ..."
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
        let path = stdlib.join("std/core/flow.sta");
        let source = std::fs::read_to_string(&path).unwrap();
        let program = ProgramLoader::new()
            .with_standard_library_root(stdlib)
            .load_source_at(&path, &source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let module = parse(&source).unwrap();
        let entries = entries(&module, &typed);
        let signatures = entries.iter().map(|entry| (&source[entry.range.clone()], entry.signature.as_str())).collect::<Vec<_>>();

        assert!(signatures.iter().any(|entry| entry.0 == "otherwise" && entry.1.starts_with("let otherwise:")), "{signatures:?}");
        assert!(signatures.iter().any(|entry| entry.0 == "body" && entry.1.starts_with("body: Braced")), "{signatures:?}");
        assert!(signatures.iter().any(|entry| entry.0 == "if_clauses" && entry.1.starts_with("def if_clauses:")), "{signatures:?}");
        assert!(entries.iter().any(|entry| {
            entry.range.start < 801
                && &source[entry.range.clone()] == "Expr"
                && entry.signature.starts_with("type alias Expr")
        }), "{signatures:?}");
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
        let signatures = entries.iter().map(|entry| (&source[entry.range.clone()], entry.signature.as_str())).collect::<Vec<_>>();

        assert!(signatures.contains(&("original", "let original: CallExpr")), "{signatures:?}");
        assert!(signatures.contains(&("changed", "let mut changed: CallExpr")), "{signatures:?}");
        assert!(signatures.contains(&("CallExpr", "(callee: Expr, argument: Expr) -> CallExpr")), "{signatures:?}");
        assert!(!signatures.iter().any(|entry| entry.1.contains("<compile-time") || entry.1.contains("<macro parameter>") || entry.1.contains("<syntax category>")));
    }
}
