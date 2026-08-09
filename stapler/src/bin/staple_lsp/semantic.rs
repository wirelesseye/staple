use std::collections::HashMap;

use lsp_types::{SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend};
use stapler::*;

pub const NAMESPACE: u32 = 0;
pub const TYPE: u32 = 1;
pub const TYPE_PARAMETER: u32 = 2;
pub const INTERFACE: u32 = 3;
pub const MACRO: u32 = 4;
pub const FUNCTION: u32 = 5;
pub const PARAMETER: u32 = 6;
pub const VARIABLE: u32 = 7;
pub const PROPERTY: u32 = 8;
pub const KEYWORD: u32 = 9;
pub const COMMENT: u32 = 10;
pub const STRING: u32 = 11;
pub const NUMBER: u32 = 12;
pub const OPERATOR: u32 = 13;

const DECLARATION: u32 = 1 << 0;
const DEFINITION: u32 = 1 << 1;
const READONLY: u32 = 1 << 2;
const MODIFICATION: u32 = 1 << 3;

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::TYPE,
            SemanticTokenType::TYPE_PARAMETER,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::MACRO,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::OPERATOR,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::MODIFICATION,
        ],
    }
}

#[derive(Clone, Copy)]
struct RawToken {
    start: usize,
    end: usize,
    kind: u32,
    modifiers: u32,
    priority: u8,
}

pub fn tokens(
    source: &str,
    module: Option<&Module>,
    resolved: Option<&ResolvedModule>,
) -> Vec<SemanticToken> {
    let mut classifier = Classifier::new(source);
    if let Some(module) = module {
        classifier.module(module, resolved);
    }
    classifier.encode()
}

struct Classifier<'a> {
    source: &'a str,
    tokens: HashMap<(usize, usize), RawToken>,
    symbols: HashMap<SymbolId, u32>,
}

impl<'a> Classifier<'a> {
    fn new(source: &'a str) -> Self {
        let mut this = Self {
            source,
            tokens: HashMap::new(),
            symbols: HashMap::new(),
        };
        for token in lex(source) {
            let kind = match token.kind {
                TokenKind::LineComment => Some(COMMENT),
                TokenKind::String => Some(STRING),
                TokenKind::Integer => Some(NUMBER),
                TokenKind::Use
                | TokenKind::As
                | TokenKind::Pub
                | TokenKind::Let
                | TokenKind::Mut
                | TokenKind::Return
                | TokenKind::Loop
                | TokenKind::Break
                | TokenKind::Continue
                | TokenKind::Def
                | TokenKind::Extern
                | TokenKind::Type
                | TokenKind::Macro
                | TokenKind::Trait
                | TokenKind::Impl
                | TokenKind::Match
                | TokenKind::Alias
                | TokenKind::Opaque
                | TokenKind::Infix
                | TokenKind::Infixl
                | TokenKind::Infixr
                | TokenKind::Underscore => Some(KEYWORD),
                TokenKind::Satisfies
                | TokenKind::Operator
                | TokenKind::Equals
                | TokenKind::Arrow
                | TokenKind::FatArrow
                | TokenKind::Ellipsis
                | TokenKind::Dollar
                | TokenKind::Star
                | TokenKind::Plus
                | TokenKind::Minus
                | TokenKind::Slash
                | TokenKind::Backtick => Some(OPERATOR),
                _ => None,
            };
            if let Some(kind) = kind {
                this.insert(token.span.start, token.span.end, kind, 0, 0);
            }
        }
        this
    }

    fn module(&mut self, module: &Module, resolved: Option<&ResolvedModule>) {
        if let Some(resolved) = resolved {
            self.collect_global_symbols(module, resolved);
        }
        for item in &module.items {
            self.item(item, resolved);
        }
        if let Some(resolved) = resolved {
            self.resolved_references(module, resolved);
        }
    }

    fn collect_global_symbols(&mut self, module: &Module, resolved: &ResolvedModule) {
        for item in &module.items {
            match item {
                Item::ExternBlock(block) => {
                    for binding in &block.bindings {
                        self.collect_binding_symbol(binding, resolved);
                    }
                }
                Item::Statement(statement) => match statement.as_ref() {
                    Statement::Binding(binding) => self.collect_binding_symbol(binding, resolved),
                    Statement::PatternBinding(binding) => {
                        self.collect_pattern_symbols(&binding.pattern, VARIABLE, resolved)
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    fn collect_binding_symbol(&mut self, binding: &Binding, resolved: &ResolvedModule) {
        if let Some(symbol) = resolved.symbol_for(binding.syntax.id) {
            self.symbols.insert(
                symbol,
                if binding.kind == BindingKind::Def {
                    FUNCTION
                } else {
                    VARIABLE
                },
            );
        }
    }

    fn collect_pattern_symbols(&mut self, pattern: &Pattern, kind: u32, resolved: &ResolvedModule) {
        match pattern {
            Pattern::Binding(binding) => {
                if let Some(symbol) = resolved.symbol_for(binding.syntax.id) {
                    self.symbols.insert(symbol, kind);
                }
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.collect_pattern_symbols(element, kind, resolved);
                }
            }
            Pattern::Nominal(nominal) => {
                self.collect_pattern_symbols(&nominal.argument, kind, resolved)
            }
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
        }
    }

    fn item(&mut self, item: &Item, resolved: Option<&ResolvedModule>) {
        match item {
            Item::UseDeclaration(value) => {
                for part in &value.path {
                    self.mark_first(&value.syntax, part, NAMESPACE, 0, 1);
                }
                match &value.kind {
                    UseKind::Renamed { alias, .. } => {
                        self.mark_last(&value.syntax, alias, VARIABLE, DECLARATION, 1)
                    }
                    UseKind::Selected(names) => {
                        for name in names {
                            self.mark_last(&value.syntax, name, VARIABLE, 0, 1);
                        }
                    }
                    _ => {}
                }
            }
            Item::ExternBlock(value) => {
                for binding in &value.bindings {
                    self.binding(binding, resolved);
                }
            }
            Item::TypeDeclaration(value) => {
                self.mark_declaration(&value.syntax, &value.name, TYPE, None, resolved);
                for parameter in &value.type_parameters {
                    self.type_parameter(parameter, resolved);
                }
                for bound in &value.trait_bounds {
                    self.trait_bound(bound, resolved);
                }
                if let Some(ty) = &value.underlying {
                    self.ty(ty, resolved);
                }
            }
            Item::MacroDeclaration(value) => {
                self.mark_first(
                    &value.syntax,
                    &value.name,
                    MACRO,
                    DECLARATION | DEFINITION | READONLY,
                    1,
                );
                if let Some(annotation) = &value.annotation {
                    self.ty(annotation, resolved);
                }
                if let Some(expression) = &value.value {
                    self.expression(expression, resolved);
                }
            }
            Item::TraitDeclaration(value) => {
                self.mark_first(
                    &value.syntax,
                    &value.name,
                    INTERFACE,
                    DECLARATION | DEFINITION | READONLY,
                    1,
                );
                self.mark_last(
                    &value.parameter.syntax,
                    &value.parameter.name,
                    TYPE_PARAMETER,
                    DECLARATION | DEFINITION | READONLY,
                    1,
                );
                for member in &value.members {
                    self.mark_first(
                        &member.syntax,
                        &member.name,
                        FUNCTION,
                        DECLARATION | READONLY,
                        1,
                    );
                    self.ty(&member.annotation, resolved);
                }
            }
            Item::TraitImplementation(value) => {
                self.mark_last(
                    &value.trait_name.syntax,
                    &value.trait_name.name,
                    INTERFACE,
                    0,
                    1,
                );
                self.ty(&value.target, resolved);
                for member in &value.members {
                    self.mark_first(
                        &member.syntax,
                        &member.name,
                        FUNCTION,
                        DEFINITION | READONLY,
                        1,
                    );
                    self.expression(&member.value, resolved);
                }
            }
            Item::Statement(value) => self.statement(value, resolved),
        }
    }

    fn statement(&mut self, statement: &Statement, resolved: Option<&ResolvedModule>) {
        match statement {
            Statement::Binding(value) => self.binding(value, resolved),
            Statement::PatternBinding(value) => {
                self.pattern(&value.pattern, VARIABLE, resolved);
                self.expression(&value.value, resolved);
            }
            Statement::Assignment(value) => {
                self.expression_with_mod(&value.target, resolved, MODIFICATION);
                self.expression(&value.value, resolved);
            }
            Statement::Return(value) => self.expression(&value.value, resolved),
            Statement::Break(value) => {
                if let Some(expression) = &value.value {
                    self.expression(expression, resolved);
                }
            }
            Statement::Continue(_) => {}
            Statement::Expression(value) => self.expression(value, resolved),
        }
    }

    fn binding(&mut self, binding: &Binding, resolved: Option<&ResolvedModule>) {
        let kind = if binding.kind == BindingKind::Def {
            FUNCTION
        } else {
            VARIABLE
        };
        let modifiers = DECLARATION | DEFINITION | if binding.mutable { 0 } else { READONLY };
        self.mark_declaration(
            &binding.syntax,
            &binding.name,
            kind,
            Some(modifiers),
            resolved,
        );
        for parameter in &binding.type_parameters {
            self.type_parameter(parameter, resolved);
        }
        for bound in &binding.trait_bounds {
            self.trait_bound(bound, resolved);
        }
        if let Some(annotation) = &binding.annotation {
            self.ty(annotation, resolved);
        }
        if let Some(value) = &binding.value {
            self.expression(value, resolved);
        }
    }

    fn mark_declaration(
        &mut self,
        syntax: &Syntax,
        name: &str,
        kind: u32,
        modifiers: Option<u32>,
        resolved: Option<&ResolvedModule>,
    ) {
        self.mark_first(
            syntax,
            name,
            kind,
            modifiers.unwrap_or(DECLARATION | DEFINITION | READONLY),
            1,
        );
        if let Some(symbol) = resolved.and_then(|module| module.symbol_for(syntax.id)) {
            self.symbols.insert(symbol, kind);
        }
    }

    fn expression(&mut self, expression: &Expression, resolved: Option<&ResolvedModule>) {
        self.expression_with_mod(expression, resolved, 0);
    }

    fn expression_with_mod(
        &mut self,
        expression: &Expression,
        resolved: Option<&ResolvedModule>,
        modifiers: u32,
    ) {
        match expression {
            Expression::Function(value) => {
                self.pattern(&value.pattern, PARAMETER, resolved);
                self.expression(&value.body, resolved);
            }
            Expression::Satisfies(value) => {
                self.expression(&value.value, resolved);
                self.ty(&value.ty, resolved);
            }
            Expression::Match(value) => {
                self.expression(&value.subject, resolved);
                for arm in &value.arms {
                    self.pattern(&arm.pattern, VARIABLE, resolved);
                    self.expression(&arm.body, resolved);
                }
            }
            Expression::Loop(value) => {
                for statement in &value.body.statements {
                    self.statement(statement, resolved);
                }
            }
            Expression::Block(value) => {
                for statement in &value.statements {
                    self.statement(statement, resolved);
                }
            }
            Expression::Product(value) => {
                for element in &value.elements {
                    if let Some(name) = &element.name {
                        self.mark_first(&element.syntax, name, PROPERTY, DECLARATION, 1);
                    }
                    self.expression(&element.value, resolved);
                }
            }
            Expression::Call(value) => {
                self.expression(&value.callee, resolved);
                self.expression(&value.argument, resolved);
            }
            Expression::Access(value) => {
                self.expression_with_mod(&value.value, resolved, modifiers);
                if let Accessor::Name(name) = &value.accessor {
                    self.mark_last(&value.syntax, name, PROPERTY, modifiers, 1);
                }
            }
            Expression::Index(value) => {
                self.expression_with_mod(&value.value, resolved, modifiers);
                self.expression(&value.index, resolved);
            }
            Expression::Infix(value) => {
                for operand in &value.operands {
                    self.expression(operand, resolved);
                }
                for operator in &value.operators {
                    self.mark_last(&operator.syntax, &operator.name, OPERATOR, 0, 1);
                }
            }
            Expression::Quote(value) => self.expression(&value.template, resolved),
            Expression::Splice(value) => self.mark_last(&value.syntax, &value.name, VARIABLE, 0, 1),
            Expression::Name(value) => {
                let kind = resolved
                    .and_then(|module| module.symbol_for(value.syntax.id))
                    .and_then(|symbol| self.symbols.get(&symbol).copied())
                    .unwrap_or(VARIABLE);
                let readonly = resolved
                    .and_then(|module| module.symbol_for(value.syntax.id))
                    .is_some_and(|symbol| !resolved.unwrap().is_mutable_symbol(symbol));
                self.mark_last(
                    &value.syntax,
                    &value.name,
                    kind,
                    modifiers | if readonly { READONLY } else { 0 },
                    2,
                );
            }
            Expression::String(_) | Expression::CString(_) | Expression::Integer(_) => {}
        }
    }

    fn pattern(&mut self, pattern: &Pattern, kind: u32, resolved: Option<&ResolvedModule>) {
        match pattern {
            Pattern::Binding(value) => {
                self.mark_last(
                    &value.syntax,
                    &value.name,
                    kind,
                    DECLARATION | DEFINITION | if value.mutable { 0 } else { READONLY },
                    1,
                );
                if let Some(symbol) = resolved.and_then(|module| module.symbol_for(value.syntax.id))
                {
                    self.symbols.insert(symbol, kind);
                }
                self.ty(&value.ty, resolved);
            }
            Pattern::Product(value) => {
                for element in &value.elements {
                    self.pattern(element, kind, resolved);
                }
            }
            Pattern::Nominal(value) => {
                self.mark_first(&value.syntax, &value.name, TYPE, 0, 1);
                self.pattern(&value.argument, kind, resolved);
            }
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) => {}
        }
    }

    fn type_parameter(
        &mut self,
        parameter: &TypeParameterPattern,
        resolved: Option<&ResolvedModule>,
    ) {
        match parameter {
            TypeParameterPattern::Binding(value) => self.mark_last(
                &value.syntax,
                &value.name,
                TYPE_PARAMETER,
                DECLARATION | DEFINITION | READONLY,
                1,
            ),
            TypeParameterPattern::Product(value) => {
                for element in &value.elements {
                    self.type_parameter(element, resolved);
                }
            }
        }
        let _ = resolved;
    }

    fn trait_bound(&mut self, bound: &TraitBound, resolved: Option<&ResolvedModule>) {
        self.mark_last(
            &bound.trait_name.syntax,
            &bound.trait_name.name,
            INTERFACE,
            0,
            1,
        );
        self.ty(&bound.argument, resolved);
    }

    fn ty(&mut self, ty: &Type, resolved: Option<&ResolvedModule>) {
        match ty {
            Type::Named(value) => {
                if let Some(namespace) = &value.namespace {
                    self.mark_first(&value.syntax, namespace, NAMESPACE, 0, 1);
                }
                self.mark_last(
                    &value.syntax,
                    &value.name,
                    if resolved
                        .and_then(|m| m.type_parameter_for(value.syntax.id))
                        .is_some()
                    {
                        TYPE_PARAMETER
                    } else {
                        TYPE
                    },
                    0,
                    1,
                );
            }
            Type::Product(value) => {
                for element in &value.elements {
                    if let Some(name) = &element.name {
                        self.mark_first(&element.syntax, name, PROPERTY, DECLARATION, 1);
                    }
                    self.ty(&element.ty, resolved);
                }
            }
            Type::Sum(value) => {
                for ty in &value.alternatives {
                    self.ty(ty, resolved);
                }
            }
            Type::Function(value) => {
                self.ty(&value.parameter, resolved);
                self.ty(&value.result, resolved);
            }
            Type::Application(value) => {
                self.ty(&value.callee, resolved);
                self.ty(&value.argument, resolved);
            }
            Type::Repeated(value) => self.ty(&value.element, resolved),
            Type::Inferred(_) | Type::StringLiteral(_) => {}
        }
    }

    fn resolved_references(&mut self, _module: &Module, _resolved: &ResolvedModule) {
        // References are classified during the recursive pass after declaration
        // symbols have been collected from source order. Forward global symbols
        // retain the syntax-aware fallback category in this first implementation.
    }

    fn mark_first(&mut self, syntax: &Syntax, name: &str, kind: u32, modifiers: u32, priority: u8) {
        if let Some(token) = syntax.tokens().iter().find(|token| {
            token.text == name
                && matches!(
                    token.kind,
                    TokenKind::Identifier
                        | TokenKind::Operator
                        | TokenKind::Plus
                        | TokenKind::Minus
                        | TokenKind::Star
                        | TokenKind::Slash
                )
        }) {
            self.insert(token.span.start, token.span.end, kind, modifiers, priority);
        }
    }

    fn mark_last(&mut self, syntax: &Syntax, name: &str, kind: u32, modifiers: u32, priority: u8) {
        if let Some(token) = syntax.tokens().iter().rev().find(|token| {
            token.text == name
                && matches!(
                    token.kind,
                    TokenKind::Identifier
                        | TokenKind::Operator
                        | TokenKind::Plus
                        | TokenKind::Minus
                        | TokenKind::Star
                        | TokenKind::Slash
                )
        }) {
            self.insert(token.span.start, token.span.end, kind, modifiers, priority);
        }
    }

    fn insert(&mut self, start: usize, end: usize, kind: u32, modifiers: u32, priority: u8) {
        let token = RawToken {
            start,
            end,
            kind,
            modifiers,
            priority,
        };
        self.tokens
            .entry((start, end))
            .and_modify(|current| {
                if priority >= current.priority {
                    *current = token;
                }
            })
            .or_insert(token);
    }

    fn encode(self) -> Vec<SemanticToken> {
        let mut raw = self.tokens.into_values().collect::<Vec<_>>();
        raw.sort_by_key(|token| (token.start, token.end));
        let mut absolute = Vec::new();
        for token in raw {
            for (start, end) in split_lines(self.source, token.start, token.end) {
                let (line, column) = position(self.source, start);
                let length = self.source[start..end].encode_utf16().count() as u32;
                if length > 0 {
                    absolute.push((line, column, length, token.kind, token.modifiers));
                }
            }
        }
        absolute.sort_by_key(|token| (token.0, token.1));
        let mut previous_line = 0;
        let mut previous_column = 0;
        absolute
            .into_iter()
            .map(|(line, column, length, kind, modifiers)| {
                let delta_line = line - previous_line;
                let delta_start = if delta_line == 0 {
                    column - previous_column
                } else {
                    column
                };
                previous_line = line;
                previous_column = column;
                SemanticToken {
                    delta_line,
                    delta_start,
                    length,
                    token_type: kind,
                    token_modifiers_bitset: modifiers,
                }
            })
            .collect()
    }
}

fn split_lines(source: &str, mut start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    while start < end {
        let tail = &source[start..end];
        let line_end = tail
            .find(['\r', '\n'])
            .map(|offset| start + offset)
            .unwrap_or(end);
        if line_end > start {
            ranges.push((start, line_end));
        }
        if line_end == end {
            break;
        }
        start = line_end + 1;
        if source.as_bytes().get(line_end) == Some(&b'\r')
            && source.as_bytes().get(start) == Some(&b'\n')
        {
            start += 1;
        }
    }
    ranges
}

pub fn position(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 0;
    let mut column = 0;
    let mut bytes = 0;
    let mut previous_was_cr = false;
    for character in source.chars() {
        if bytes >= offset {
            break;
        }
        bytes += character.len_utf8();
        match character {
            '\n' => {
                if !previous_was_cr {
                    line += 1;
                }
                column = 0;
            }
            '\r' => {
                line += 1;
                column = 0;
            }
            _ => column += character.len_utf16() as u32,
        }
        previous_was_cr = character == '\r';
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn legend_order_is_stable() {
        let legend = legend();
        assert_eq!(
            legend.token_types[FUNCTION as usize],
            SemanticTokenType::FUNCTION
        );
        assert_eq!(
            legend.token_types[COMMENT as usize],
            SemanticTokenType::COMMENT
        );
        assert_eq!(
            legend.token_modifiers[0],
            SemanticTokenModifier::DECLARATION
        );
    }

    #[test]
    fn malformed_source_keeps_lexical_tokens() {
        let result = tokens("def broken = \"unterminated", None, None);
        assert!(result.iter().any(|token| token.token_type == KEYWORD));
        assert!(result.iter().any(|token| token.token_type == STRING));
    }

    #[test]
    fn positions_use_utf16_and_crlf_lines() {
        assert_eq!(position("😀x\r\ny", "😀".len()), (0, 2));
        assert_eq!(position("😀x\r\ny", "😀x\r\n".len()), (1, 0));
        assert_eq!(position("x\ry", "x\r".len()), (1, 0));
    }

    #[test]
    fn classifies_core_ast_roles() {
        let source = concat!(
            "use tools\n",
            "type alias Item = I32\n",
            "trait Show = T => { show: T -> String }\n",
            "macro identity = value => quote { $value }\n",
            "def project: T => T -> T = value => (field: value).field\n",
        );
        let module = parse(source).unwrap();
        let labels = labels(source, &tokens(source, Some(&module), None));
        for expected in [
            ("tools", NAMESPACE),
            ("Item", TYPE),
            ("Show", INTERFACE),
            ("identity", MACRO),
            ("project", FUNCTION),
            ("T", TYPE_PARAMETER),
            ("value", PARAMETER),
            ("field", PROPERTY),
        ] {
            assert!(
                labels.contains(&expected),
                "missing {expected:?} in {labels:?}"
            );
        }
    }

    #[test]
    fn resolution_propagates_forward_function_kinds() {
        let source = "def first = () => second ()\ndef second = () => 1\n";
        let path = std::env::temp_dir().join("staple-semantic-forward.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let module = parse(source).unwrap();
        let labels = labels(source, &tokens(source, Some(&module), Some(&resolved)));
        assert!(
            labels
                .iter()
                .filter(|token| **token == ("second", FUNCTION))
                .count()
                >= 2
        );
    }

    fn labels<'a>(source: &'a str, tokens: &[SemanticToken]) -> Vec<(&'a str, u32)> {
        let lines = source.split('\n').collect::<Vec<_>>();
        let mut line = 0;
        let mut column = 0;
        let mut result = Vec::new();
        for token in tokens {
            line += token.delta_line;
            column = if token.delta_line == 0 {
                column + token.delta_start
            } else {
                token.delta_start
            };
            let text = &lines[line as usize][column as usize..(column + token.length) as usize];
            result.push((text, token.token_type));
        }
        result
    }
}
