//! Reference scanning over raw syntax.
//!
//! Collects the names an item's syntax refers to, with local scopes tracked so
//! that a name bound by a `let`, a parameter or a pattern is not mistaken for a
//! module-level item. References inside a quotation are generated code rather
//! than something the item itself needs when it runs, so they are kept apart.
use super::*;
use staple_syntax::TypeParameterPattern;

/// A written name: a dotted path with the module its hygiene resolves it in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RawRef {
    pub(super) path: Vec<String>,
    pub(super) definition: Option<ModuleId>,
}

#[derive(Default, Debug)]
pub(super) struct Scan {
    /// Names the item needs at run time.
    pub(super) refs: Vec<RawRef>,
    /// Names written inside quotations.
    pub(super) quoted: Vec<RawRef>,
    /// Modifier names applied to items.
    pub(super) modifiers: Vec<RawRef>,
    /// Names used as methods or pipe stages, which resolve against the
    /// receiver's companion functions rather than against the scope.
    pub(super) methods: Vec<String>,
}

impl Scan {
    pub(super) fn item(item: &Item, modules: StdModules) -> Self {
        let mut scanner = Scanner::new(modules);
        scanner.top_level_item(item);
        scanner.scan
    }

    pub(super) fn expression(
        expression: &Expression,
        locals: &[String],
        modules: StdModules,
    ) -> Self {
        let mut scanner = Scanner::new(modules);
        scanner.frames.push(Vec::new());
        for name in locals {
            scanner.bind(name);
        }
        scanner.expression(expression);
        scanner.scan
    }
}

struct Scanner {
    scan: Scan,
    /// How many enclosing scopes bind each name, so a lookup hashes once.
    bound: HashMap<String, u32>,
    /// The names each open scope binds, to unbind them when it closes.
    frames: Vec<Vec<String>>,
    quote_depth: usize,
    /// The standard-library modules operator syntax resolves against.
    modules: StdModules,
}

impl Scanner {
    fn new(modules: StdModules) -> Self {
        Self {
            scan: Scan::default(),
            bound: HashMap::new(),
            frames: Vec::new(),
            quote_depth: 0,
            modules,
        }
    }

    /// A name an operator stands for, resolved in the module that defines it
    /// whatever the surrounding module imports.
    fn standard(&mut self, name: &str, module: Option<ModuleId>) {
        if self.quote_depth > 0 {
            return;
        }
        self.scan.refs.push(RawRef {
            path: vec![name.to_owned()],
            definition: module,
        });
    }

    fn is_local(&self, name: &str) -> bool {
        self.bound.contains_key(name)
    }

    fn bind(&mut self, name: &str) {
        if let Some(frame) = self.frames.last_mut() {
            frame.push(name.to_owned());
            *self.bound.entry(name.to_owned()).or_insert(0) += 1;
        }
    }

    fn reference(&mut self, path: Vec<String>, syntax: &Syntax) {
        if path.first().is_none_or(|first| self.is_local(first)) {
            return;
        }
        let reference = RawRef {
            path,
            definition: syntax.definition_module().map(ModuleId),
        };
        if self.quote_depth > 0 {
            self.scan.quoted.push(reference);
        } else {
            self.scan.refs.push(reference);
        }
    }

    fn method(&mut self, name: &str) {
        if self.quote_depth == 0 && !self.scan.methods.iter().any(|known| known == name) {
            self.scan.methods.push(name.to_owned());
        }
    }

    fn named_type(&mut self, named: &staple_syntax::NamedType) {
        let mut path = Vec::new();
        if let Some(namespace) = &named.namespace {
            path.extend(namespace.split('.').map(str::to_owned));
        }
        path.push(named.name.clone());
        self.reference(path, &named.syntax);
    }

    fn scoped(&mut self, body: impl FnOnce(&mut Self)) {
        self.frames.push(Vec::new());
        body(self);
        for name in self.frames.pop().unwrap_or_default() {
            if let Some(count) = self.bound.get_mut(&name) {
                *count -= 1;
                if *count == 0 {
                    self.bound.remove(&name);
                }
            }
        }
    }

    // ----- items -----

    fn top_level_item(&mut self, item: &Item) {
        self.scoped(|scanner| scanner.item(item));
    }

    fn modifiers(&mut self, modifiers: &[staple_syntax::ModifierInvocation]) {
        for modifier in modifiers {
            let mut path = Vec::new();
            if let Some(namespace) = &modifier.namespace {
                path.extend(namespace.split('.').map(str::to_owned));
            }
            path.push(modifier.name.clone());
            self.scan.modifiers.push(RawRef {
                path,
                definition: modifier.syntax.definition_module().map(ModuleId),
            });
            if let Some(argument) = &modifier.argument
                && let Some(expression) = &argument.expression
            {
                self.expression(expression);
            }
        }
    }

    fn generics(&mut self, parameters: &[TypeParameterPattern]) {
        for parameter in parameters {
            for name in parameter.names() {
                self.bind(name);
            }
        }
    }

    fn bounds(
        &mut self,
        trait_bounds: &[staple_syntax::TraitBound],
        subtype_bounds: &[staple_syntax::SubtypeBound],
        equality_bounds: &[staple_syntax::EqualityBound],
    ) {
        for bound in trait_bounds {
            self.named_type(&bound.trait_name);
            for argument in &bound.arguments {
                self.ty(argument);
            }
        }
        for bound in subtype_bounds {
            self.ty(&bound.supertype);
        }
        for bound in equality_bounds {
            self.ty(&bound.left);
            self.ty(&bound.right);
        }
    }

    fn generic_parameters(&mut self, generics: &staple_syntax::GenericParameters) {
        self.generics(&generics.parameters);
        self.bounds(
            &generics.trait_bounds,
            &generics.subtype_bounds,
            &generics.equality_bounds,
        );
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Modified(modified) => {
                self.modifiers(&modified.modifiers);
                self.item(&modified.item);
            }
            Item::VisibilityMacroInvocation(invocation) => {
                self.modifiers(&invocation.modifiers);
                self.expression(&invocation.expression);
            }
            Item::VisibilitySplice(splice) => self.item(&splice.item),
            Item::RepeatedItemSplice(_) | Item::UseDeclaration(_) | Item::Continue(_) => {}
            Item::Submodule(submodule) => {
                self.scoped(|scanner| {
                    scanner.generics(&submodule.type_parameters);
                    scanner.bounds(
                        &submodule.trait_bounds,
                        &submodule.subtype_bounds,
                        &submodule.equality_bounds,
                    );
                    if let Some(target) = &submodule.companion_target {
                        scanner.ty(target);
                    }
                    for item in &submodule.module.items {
                        scanner.item(item);
                    }
                });
            }
            Item::ExternBlock(block) => {
                for binding in &block.bindings {
                    self.binding(binding);
                }
            }
            Item::TypeDeclaration(declaration) => self.type_declaration(declaration),
            Item::MacroDeclaration(declaration) => {
                self.scoped(|scanner| {
                    scanner.generics(&declaration.type_parameters);
                    scanner.bounds(
                        &declaration.trait_bounds,
                        &declaration.subtype_bounds,
                        &declaration.equality_bounds,
                    );
                    if let Some(annotation) = &declaration.annotation {
                        scanner.ty(annotation);
                    }
                    if let Some(value) = &declaration.value {
                        scanner.expression(value);
                    }
                });
            }
            Item::TraitDeclaration(declaration) => {
                self.scoped(|scanner| {
                    scanner.generics(&declaration.type_parameters);
                    for associated in &declaration.associated_types {
                        scanner.bind(&associated.name);
                    }
                    scanner.bounds(
                        &declaration.prerequisites,
                        &declaration.subtype_bounds,
                        &declaration.equality_bounds,
                    );
                    for associated in &declaration.associated_types {
                        scanner.bounds(
                            &associated.trait_bounds,
                            &associated.subtype_bounds,
                            &associated.equality_bounds,
                        );
                    }
                    for member in &declaration.members {
                        scanner.modifiers(&member.modifiers);
                        scanner.ty(&member.annotation);
                        if let Some(default) = &member.default {
                            scanner.expression(default);
                        }
                    }
                });
            }
            Item::TraitImplementation(implementation) => {
                self.scoped(|scanner| {
                    scanner.generics(&implementation.type_parameters);
                    scanner.bounds(
                        &implementation.trait_bounds,
                        &implementation.subtype_bounds,
                        &implementation.equality_bounds,
                    );
                    scanner.named_type(&implementation.trait_name);
                    for argument in &implementation.arguments {
                        scanner.ty(argument);
                    }
                    for associated in &implementation.associated_types {
                        scanner.ty(&associated.value);
                    }
                    for member in &implementation.members {
                        scanner.modifiers(&member.modifiers);
                        scanner.expression(&member.value);
                    }
                });
            }
            Item::Binding(binding) => {
                self.binding(binding);
                self.bind(&binding.name);
            }
            Item::PatternBinding(binding) => {
                self.expression(&binding.value);
                self.pattern(&binding.pattern);
            }
            Item::Assignment(assignment) => {
                self.expression(&assignment.target);
                self.expression(&assignment.value);
            }
            Item::Return(item) => self.expression(&item.value),
            Item::Break(item) => {
                if let Some(value) = &item.value {
                    self.expression(value);
                }
            }
            Item::Expression(expression) => self.expression(expression),
        }
    }

    fn type_declaration(&mut self, declaration: &staple_syntax::TypeDeclaration) {
        self.scoped(|scanner| {
            scanner.generics(&declaration.type_parameters);
            scanner.bounds(
                &declaration.trait_bounds,
                &declaration.subtype_bounds,
                &declaration.equality_bounds,
            );
            for bound in &declaration.default_bounds {
                scanner.ty(&bound.default);
            }
            if let Some(underlying) = declaration.underlying() {
                scanner.ty(underlying);
            }
            if let Some(effects) = &declaration.resource_dependencies {
                scanner.effects(effects);
            }
        });
    }

    fn binding(&mut self, binding: &staple_syntax::Binding) {
        self.scoped(|scanner| {
            scanner.modifiers(&binding.member_modifiers);
            if let Some(generics) = &binding.companion_generics {
                scanner.generic_parameters(generics);
            }
            if let Some(target) = &binding.companion_target {
                scanner.ty(target);
            }
            if let Some(annotation) = &binding.annotation {
                scanner.ty(annotation);
            }
            if let Some(value) = &binding.value {
                scanner.expression(value);
            }
        });
    }

    // ----- types -----

    fn effects(&mut self, effects: &staple_syntax::EffectSet) {
        for resource in &effects.resources {
            self.ty(&resource.value_type);
        }
    }

    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Inferred(_)
            | Type::NumberLiteral(_)
            | Type::StringLiteral(_)
            | Type::Splice(_) => {}
            Type::Named(named) => self.named_type(named),
            Type::Product(product) => {
                for element in &product.elements {
                    self.ty(&element.ty);
                    if let Some(default) = &element.default {
                        self.expression(default);
                    }
                }
            }
            Type::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.ty(alternative);
                }
            }
            Type::Function(function) => {
                self.ty(&function.parameter);
                self.effects(&function.effects);
                self.ty(&function.result);
            }
            Type::Forall(forall) => self.scoped(|scanner| {
                scanner.generic_parameters(&forall.generics);
                scanner.ty(&forall.body);
            }),
            Type::Application(application) => {
                self.ty(&application.callee);
                self.ty(&application.argument);
            }
            Type::EffectApplication(application) => {
                self.ty(&application.callee);
                self.effects(&application.effects);
            }
            Type::Array(array) => {
                self.ty(&array.element);
                self.ty(&array.count);
            }
        }
    }

    // ----- patterns -----

    /// Walks a pattern, binding the names it introduces. A capitalized name, or
    /// a constructor, refers to an item instead.
    fn pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Binding(binding) => {
                if binding.name.chars().next().is_some_and(char::is_uppercase) {
                    let name = binding
                        .resolution_name
                        .clone()
                        .unwrap_or_else(|| binding.name.clone());
                    self.reference(vec![name], &binding.syntax);
                } else {
                    self.bind(&binding.name);
                }
                self.ty(&binding.ty);
            }
            Pattern::At(at) => {
                self.bind(&at.binding.name);
                self.ty(&at.binding.ty);
                self.pattern(&at.pattern);
            }
            Pattern::Wildcard(wildcard) => self.ty(&wildcard.ty),
            Pattern::StringLiteral(_) | Pattern::CharLiteral(_) | Pattern::Splice(_) => {}
            Pattern::Product(product) => {
                if let Some(annotation) = &product.annotation {
                    self.ty(annotation);
                }
                for element in &product.elements {
                    self.pattern(element);
                }
            }
            Pattern::Nominal(nominal) => {
                let mut path = Vec::new();
                if let Some(namespace) = &nominal.namespace {
                    path.extend(namespace.split('.').map(str::to_owned));
                }
                path.push(nominal.name.clone());
                self.reference(path, &nominal.syntax);
                if let Some(annotation) = &nominal.annotation {
                    self.ty(annotation);
                }
                self.pattern(&nominal.argument);
            }
        }
    }

    // ----- expressions -----

    fn block(&mut self, block: &staple_syntax::BlockExpression) {
        self.scoped(|scanner| {
            for item in &block.items {
                scanner.item(item);
            }
        });
    }

    /// The dotted path an expression spells out, if it is only names.
    fn path(expression: &Expression) -> Option<Vec<String>> {
        match expression {
            Expression::Name(name) => Some(vec![name.name.clone()]),
            Expression::Access(access) => {
                let Accessor::Name(member) = &access.accessor else {
                    return None;
                };
                let mut path = Self::path(&access.value)?;
                path.push(member.clone());
                Some(path)
            }
            _ => None,
        }
    }

    fn expression(&mut self, expression: &Expression) {
        match expression {
            Expression::Function(function) => self.scoped(|scanner| {
                if let Some(generics) = &function.generics {
                    scanner.generic_parameters(generics);
                }
                scanner.pattern(&function.pattern);
                scanner.expression(&function.body);
            }),
            Expression::TypeAscription(ascription) => {
                self.expression(&ascription.value);
                self.ty(&ascription.ty);
            }
            Expression::TypeApplication(application) => {
                self.expression(&application.value);
                for argument in &application.arguments {
                    self.ty(argument);
                }
            }
            Expression::Match(match_) => {
                self.expression(&match_.subject);
                for arm in &match_.arms {
                    self.scoped(|scanner| {
                        scanner.pattern(&arm.pattern);
                        scanner.expression(&arm.body);
                    });
                }
            }
            Expression::Loop(loop_) => self.block(&loop_.body),
            Expression::Coro(coro) => self.block(&coro.body),
            Expression::Await(await_) => self.expression(&await_.operand),
            Expression::Resource(resource) => self.ty(&resource.resource),
            Expression::With(with) => {
                self.ty(&with.resource);
                self.expression(&with.value);
                self.block(&with.body);
            }
            Expression::Block(block) => self.block(block),
            Expression::Product(product) => {
                for element in &product.elements {
                    self.expression(&element.value);
                }
            }
            Expression::RepeatedProduct(repeated) => {
                self.expression(&repeated.value);
                self.ty(&repeated.count);
            }
            Expression::Call(call) => {
                self.expression(&call.callee);
                self.expression(&call.argument);
            }
            Expression::Access(access) => {
                if let Some(path) = Self::path(expression) {
                    self.reference(path, &access.syntax);
                } else {
                    if let Accessor::Method(name) = &access.accessor {
                        self.method(name);
                    }
                    self.expression(&access.value);
                }
            }
            Expression::Index(index) => {
                self.standard("Index", self.modules.index);
                self.standard("MutateIndex", self.modules.index);
                self.expression(&index.value);
                self.expression(&index.index);
            }
            Expression::Unary(unary) => {
                let (trait_name, _) = unary.operator.trait_method();
                self.standard(trait_name, self.modules.ops);
                self.expression(&unary.operand);
            }
            Expression::Try(try_) => {
                self.standard("Try", self.modules.try_);
                self.standard("Ok", self.modules.result);
                self.expression(&try_.operand);
            }
            Expression::Conversion(conversion) => {
                self.standard("Convert", self.modules.convert);
                self.expression(&conversion.value);
                self.ty(&conversion.ty);
            }
            Expression::Binary(binary) => {
                use staple_syntax::BinaryOperator::*;
                match binary.operator {
                    Add => self.standard("Add", self.modules.ops),
                    Subtract => self.standard("Subtract", self.modules.ops),
                    Multiply => self.standard("Multiply", self.modules.ops),
                    Divide => self.standard("Divide", self.modules.ops),
                    Remainder => self.standard("Remainder", self.modules.ops),
                    Equal | NotEqual => self.standard("Eq", self.modules.cmp),
                    Less | LessEqual | Greater | GreaterEqual => {
                        self.standard("PartialOrd", self.modules.cmp)
                    }
                    Range => self.standard("range", self.modules.range),
                    RangeInclusive => self.standard("range_inclusive", self.modules.range),
                    And | Or => self.standard("Bool", self.modules.boolean),
                }
                self.expression(&binary.left);
                self.expression(&binary.right);
            }
            Expression::Pipe(pipe) => {
                self.expression(&pipe.receiver);
                let mut head = pipe.stage.as_ref();
                loop {
                    match head {
                        Expression::Call(call) => head = &call.callee,
                        Expression::Product(product) if product.is_grouping() => {
                            head = &product.elements[0].value
                        }
                        _ => break,
                    }
                }
                if let Expression::Name(name) = head {
                    self.method(&name.name);
                }
                self.expression(&pipe.stage);
            }
            Expression::Logical(logical) => {
                self.standard("Bool", self.modules.boolean);
                self.expression(&logical.left);
                self.expression(&logical.right);
                self.ty(&logical.bool_type);
            }
            Expression::SyntaxArgument(argument) => {
                // Unparsed tokens: every identifier is a possible reference.
                let tokens = argument.syntax.tokens();
                for (index, token) in tokens.iter().enumerate() {
                    if token.kind == staple_syntax::TokenKind::Identifier
                        && !(index > 0
                            && tokens[index - 1].kind == staple_syntax::TokenKind::Dollar)
                    {
                        self.reference(vec![token.text.clone()], &argument.syntax);
                    }
                }
            }
            Expression::VisibilityArgument(_)
            | Expression::Splice(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Char(_)
            | Expression::Byte(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
            Expression::Quote(quote) => {
                self.quote_depth += 1;
                match &quote.template {
                    staple_syntax::QuoteTemplate::Expression(value) => self.expression(value),
                    staple_syntax::QuoteTemplate::Item(item) => {
                        self.scoped(|scanner| scanner.item(item))
                    }
                    staple_syntax::QuoteTemplate::Items(items) => self.scoped(|scanner| {
                        for item in items {
                            scanner.item(item);
                        }
                    }),
                    staple_syntax::QuoteTemplate::Raw => {
                        let tokens = quote.contents.tokens();
                        for (index, token) in tokens.iter().enumerate() {
                            if token.kind == staple_syntax::TokenKind::Identifier
                                && !(index > 0
                                    && tokens[index - 1].kind == staple_syntax::TokenKind::Dollar)
                            {
                                self.reference(vec![token.text.clone()], &quote.syntax);
                            }
                        }
                    }
                }
                self.quote_depth -= 1;
            }
            Expression::Name(name) => self.reference(vec![name.name.clone()], &name.syntax),
            Expression::StringTemplate(template) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        let format = match interpolation.format {
                            staple_syntax::StringInterpolationFormat::Display => "Display",
                            staple_syntax::StringInterpolationFormat::Debug => "Debug",
                        };
                        self.standard(format, self.modules.fmt);
                        self.expression(&interpolation.expression);
                    }
                }
            }
        }
    }
}
