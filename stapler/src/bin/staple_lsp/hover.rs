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
            Item::Statement(statement) => self.collect_statement_declarations(statement),
            Item::UseDeclaration(_) | Item::TypeDeclaration(_) => {}
        }
    }

    fn collect_statement_declarations(&mut self, statement: &Statement) {
        match statement {
            Statement::Binding(binding) => self.collect_binding_declaration(binding),
            Statement::PatternBinding(binding) => {
                self.collect_pattern_declarations(&binding.pattern, Some("let"));
                self.collect_expression_declarations(&binding.value);
            }
            Statement::Assignment(assignment) => {
                self.collect_expression_declarations(&assignment.target);
                self.collect_expression_declarations(&assignment.value);
            }
            Statement::Return(return_) => self.collect_expression_declarations(&return_.value),
            Statement::Break(break_) => {
                if let Some(value) = &break_.value {
                    self.collect_expression_declarations(value);
                }
            }
            Statement::Continue(_) => {}
            Statement::Expression(expression) => self.collect_expression_declarations(expression),
        }
    }

    fn collect_binding_declaration(&mut self, binding: &Binding) {
        if let Some(symbol) = self.typed.symbol_for(binding.syntax.id) {
            self.declarations.insert(
                symbol,
                Declaration {
                    prefix: Some(match binding.kind {
                        BindingKind::Let => "let",
                        BindingKind::Def => "def",
                    }),
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
                self.collect_pattern_declarations(&function.pattern, None);
                self.collect_expression_declarations(&function.body);
            }
            Expression::Satisfies(satisfies) => {
                self.collect_expression_declarations(&satisfies.value)
            }
            Expression::Match(match_) => {
                self.collect_expression_declarations(&match_.subject);
                for arm in &match_.arms {
                    self.collect_pattern_declarations(&arm.pattern, None);
                    self.collect_expression_declarations(&arm.body);
                }
            }
            Expression::Loop(loop_) => {
                for statement in &loop_.body.statements {
                    self.collect_statement_declarations(statement);
                }
            }
            Expression::Block(block) => {
                for statement in &block.statements {
                    self.collect_statement_declarations(statement);
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
            Expression::Infix(infix) => {
                for operand in &infix.operands {
                    self.collect_expression_declarations(operand);
                }
            }
            Expression::Quote(quote) => self.collect_expression_declarations(&quote.template),
            Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn collect_pattern_declarations(&mut self, pattern: &Pattern, prefix: Option<&'static str>) {
        match pattern {
            Pattern::Binding(binding) => {
                if let Some(symbol) = self.typed.symbol_for(binding.syntax.id) {
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
                    self.collect_pattern_declarations(element, prefix);
                }
            }
            Pattern::Nominal(nominal) => {
                self.collect_pattern_declarations(&nominal.argument, prefix)
            }
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
        }
    }

    fn item(&mut self, item: &Item) {
        match item {
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
                if let Some(value) = &declaration.value {
                    self.expression(value);
                }
            }
            Item::TraitImplementation(implementation) => {
                for member in &implementation.members {
                    let value_type = self
                        .typed
                        .type_of_expression(member.value.syntax().id)
                        .map(ToString::to_string)
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
                    if let Some(default) = &member.default {
                        self.expression(default);
                    }
                }
            }
            Item::Statement(statement) => self.statement(statement),
            Item::UseDeclaration(_) | Item::TypeDeclaration(_) => {}
        }
    }

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Binding(binding) => self.binding(binding),
            Statement::PatternBinding(binding) => {
                self.pattern(&binding.pattern);
                self.expression(&binding.value);
            }
            Statement::Assignment(assignment) => {
                self.expression(&assignment.target);
                self.expression(&assignment.value);
            }
            Statement::Return(return_) => self.expression(&return_.value),
            Statement::Break(break_) => {
                if let Some(value) = &break_.value {
                    self.expression(value);
                }
            }
            Statement::Continue(_) => {}
            Statement::Expression(expression) => self.expression(expression),
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
            .map(|value_type| value_type.to_string())
    }

    fn binding(&mut self, binding: &Binding) {
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
            .map(ToString::to_string);
        if let Some(value_type) = value_type {
            let prefix = match binding.kind {
                BindingKind::Let => "let",
                BindingKind::Def => "def",
            };
            self.named(
                &binding.syntax,
                &binding.name,
                format!("{prefix} {}: {value_type}", binding.name),
            );
        }
        if let Some(value) = &binding.value {
            self.expression(value);
        }
    }

    fn expression(&mut self, expression: &Expression) {
        if let Some(value_type) = self.typed.type_of_expression(expression.syntax().id) {
            let value_type = value_type.to_string();
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
            Expression::Satisfies(satisfies) => self.expression(&satisfies.value),
            Expression::Match(match_) => {
                self.expression(&match_.subject);
                for arm in &match_.arms {
                    self.pattern(&arm.pattern);
                    self.expression(&arm.body);
                }
            }
            Expression::Loop(loop_) => {
                for statement in &loop_.body.statements {
                    self.statement(statement);
                }
            }
            Expression::Block(block) => {
                for statement in &block.statements {
                    self.statement(statement);
                }
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    self.expression(&element.value);
                    if let Some(name) = &element.name
                        && let Some(value_type) =
                            self.typed.type_of_expression(element.value.syntax().id)
                    {
                        self.named(&element.syntax, name, value_type.to_string());
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
            Expression::Infix(infix) => {
                for operand in &infix.operands {
                    self.expression(operand);
                }
            }
            Expression::Quote(quote) => self.expression(&quote.template),
            Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        if let Some(value_type) = self.typed.type_of_pattern(pattern.syntax().id) {
            self.syntax(pattern.syntax(), value_type.to_string());
            if let Pattern::Binding(binding) = pattern {
                let value_type = value_type.to_string();
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
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.pattern(element);
                }
            }
            Pattern::Nominal(nominal) => self.pattern(&nominal.argument),
            Pattern::Binding(_) | Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
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
    fn formats_def_and_trait_member_declarations() {
        let source = concat!(
            "trait Identity = T => { identity: T -> T }\n",
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
            "use dependency (imported, imported_value)\n",
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
}
