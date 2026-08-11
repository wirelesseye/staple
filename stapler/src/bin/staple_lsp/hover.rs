use std::ops::Range;

use stapler::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverEntry {
    pub range: Range<usize>,
    pub value_type: String,
}

pub fn entries(module: &Module, typed: &TypedModule) -> Vec<HoverEntry> {
    let mut collector = Collector {
        typed,
        entries: Vec::new(),
    };
    for item in &module.items {
        collector.item(item);
    }
    collector.entries
}

struct Collector<'a> {
    typed: &'a TypedModule,
    entries: Vec<HoverEntry>,
}

impl Collector<'_> {
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
                    self.expression(&member.value);
                }
            }
            Item::TraitDeclaration(declaration) => {
                for member in &declaration.members {
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
            self.named(&binding.syntax, &binding.name, value_type);
        }
        if let Some(value) = &binding.value {
            self.expression(value);
        }
    }

    fn expression(&mut self, expression: &Expression) {
        if let Some(value_type) = self.typed.type_of_expression(expression.syntax().id) {
            self.syntax(expression.syntax(), value_type.to_string());
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
                self.named(&binding.syntax, &binding.name, value_type.to_string());
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

    fn syntax(&mut self, syntax: &Syntax, value_type: String) {
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
                value_type,
            });
        }
    }

    fn named(&mut self, syntax: &Syntax, name: &str, value_type: String) {
        if let Some(token) = syntax.tokens().iter().find(|token| token.text == name) {
            self.entries.push(HoverEntry {
                range: token.span.clone(),
                value_type,
            });
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
        assert!(references.iter().all(|entry| entry.value_type == "I32"));
    }
}
