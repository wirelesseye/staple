use stapler::{
    Accessor, Expression, Item, Pattern, Statement, TokenKind, Type, TypeDeclarationKind, UseKind,
    Visibility, parse,
};

#[test]
fn parses_typed_resource_sets_accesses_and_providers_losslessly() {
    let source = concat!(
        "type Clock = (now: () -> I32)\n",
        "def read: () ->{Clock} I32 = () => (resource Clock).now ()\n",
        "def nested: () ->{Clock} () ->{} I32 = () => () => 1\n",
        "with Clock = system_clock { read () }\n",
        "macro request = () => quote { resource Clock }\n",
        "macro provide = value => quote { with Clock = $value { resource Clock } }\n",
    );
    let module = parse(source).expect("resource syntax should parse");
    assert_eq!(module.syntax.text(), source);
    let Item::Statement(statement) = &module.items[1] else {
        panic!("expected resource function declaration")
    };
    let Statement::Binding(binding) = statement.as_ref() else {
        panic!("expected binding")
    };
    let Some(Type::Function(function)) = &binding.annotation else {
        panic!("expected function annotation")
    };
    assert_eq!(function.resources.resources.len(), 1);
    assert!(matches!(module.items[3], Item::Statement(_)));
    assert!(matches!(module.items[4], Item::MacroDeclaration(_)));
    assert!(matches!(module.items[5], Item::MacroDeclaration(_)));

    assert!(parse("resource () ->\n").is_err());
    assert!(parse("with Clock = {}\n").is_err());
    assert!(parse("def bad: () ->{Clock I32 = () => 0\n").is_err());
}

#[test]
fn parses_mut_effect_sets_losslessly() {
    use stapler::{MutationTarget, MutationTargetKind};

    fn mutations(annotation: &Option<Type>) -> &[MutationTarget] {
        let Some(Type::Function(function)) = annotation else {
            panic!("expected function annotation");
        };
        &function.resources.mutations
    }

    let source = concat!(
        "def f1: A ->{mut} () = a => ()\n",
        "def f2: (A, B) ->{mut 0} () = (a, b) => ()\n",
        "def f3: (a: A, b: B) ->{mut a} () = (a, b) => ()\n",
        "def f4: (a: A, b: B) ->{mut a, IO} () = (a, b) => ()\n",
    );
    let module = parse(source).expect("mut effect sets should parse");
    assert_eq!(module.text(), source);

    let Statement::Binding(f1) = statement(&module.items[0]) else {
        panic!("expected binding");
    };
    assert!(matches!(
        mutations(&f1.annotation),
        [MutationTarget {
            target: MutationTargetKind::Whole,
            ..
        }]
    ));

    let Statement::Binding(f2) = statement(&module.items[1]) else {
        panic!("expected binding");
    };
    assert!(matches!(
        mutations(&f2.annotation),
        [MutationTarget {
            target: MutationTargetKind::Element(0),
            ..
        }]
    ));

    let Statement::Binding(f3) = statement(&module.items[2]) else {
        panic!("expected binding");
    };
    let [MutationTarget {
        target: MutationTargetKind::Named(name),
        ..
    }] = mutations(&f3.annotation)
    else {
        panic!("expected a named mutation target");
    };
    assert_eq!(name, "a");

    let Statement::Binding(f4) = statement(&module.items[3]) else {
        panic!("expected binding");
    };
    let Some(Type::Function(function)) = &f4.annotation else {
        panic!("expected function annotation");
    };
    assert_eq!(function.resources.mutations.len(), 1);
    assert_eq!(function.resources.resources.len(), 1);

    assert!(parse("def bad: A ->{mut mut} () = a => ()\n").is_err());
}

fn statement(item: &Item) -> &Statement {
    let Item::Statement(statement) = item else {
        panic!("expected statement");
    };
    statement
}

#[test]
fn parses_float_literals_losslessly_without_stealing_access_dots() {
    let source = "1.0\n1.\n.5\n1e3\n1.5e-2\n1.e2\n1.field\n(1, 2).0\n";
    let root = parse(source).expect("float literals should parse");
    assert_eq!(root.text(), source);
    for item in &root.items[..6] {
        assert!(matches!(
            statement(item),
            Statement::Expression(Expression::Float(_))
        ));
    }
    assert!(matches!(
        statement(&root.items[6]),
        Statement::Expression(Expression::Access(access))
            if matches!(access.value.as_ref(), Expression::Integer(_))
                && access.accessor == Accessor::Name("field".into())
    ));
    assert!(matches!(
        statement(&root.items[7]),
        Statement::Expression(Expression::Access(access))
            if access.accessor == Accessor::Index("0".into())
    ));
    assert_eq!(
        stapler::lex("1e+")[0].kind,
        TokenKind::Float,
        "an incomplete exponent should remain one diagnostic token"
    );
}

#[test]
fn parses_match_expressions_and_wildcards_losslessly() {
    let source = concat!(
        "def choose = result: Ok I32 | IOError => match result {\n",
        "  Ok value => value,\n",
        "  IOError (message, _) => { message },\n",
        "}\n",
        "def discard: I32 -> () = _ => ()\n",
    );
    let root = parse(source).expect("match expression should parse");
    assert_eq!(root.text(), source);
    let Statement::Binding(choose) = statement(&root.items[0]) else {
        panic!("expected choose binding");
    };
    let Some(Expression::Function(function)) = &choose.value else {
        panic!("expected choose function");
    };
    let Expression::Match(match_) = function.body.as_ref() else {
        panic!("expected match expression");
    };
    assert_eq!(match_.arms.len(), 2);
    assert!(matches!(match_.arms[0].pattern, Pattern::Nominal(_)));
    assert!(matches!(match_.arms[1].pattern, Pattern::Nominal(_)));

    let Statement::Binding(discard) = statement(&root.items[1]) else {
        panic!("expected discard binding");
    };
    let Some(Expression::Function(function)) = &discard.value else {
        panic!("expected discard function");
    };
    assert!(matches!(function.pattern, Pattern::Wildcard(_)));
}

#[test]
fn parses_typed_wildcard_parameters_losslessly() {
    let source = "def discard = _: String => ()\n";
    let root = parse(source).expect("typed wildcard parameter should parse");
    assert_eq!(root.text(), source);
    let Statement::Binding(discard) = statement(&root.items[0]) else {
        panic!("expected discard binding");
    };
    let Some(Expression::Function(function)) = &discard.value else {
        panic!("expected discard function");
    };
    assert!(matches!(
        function.pattern,
        Pattern::Wildcard(ref wildcard) if !matches!(wildcard.ty, Type::Inferred(_))
    ));
}

#[test]
fn rejects_malformed_match_expressions() {
    assert!(parse("match value {}\n").is_err());
    assert!(parse("match value { Ok x x, IOError y => y }\n").is_err());
    assert!(parse("match value { Ok x => 1 IOError y => y }\n").is_err());
    assert!(parse("match value { Ok x => x,\n").is_err());
}

#[test]
fn parses_string_literal_types_and_patterns_losslessly() {
    let source = concat!(
        "type alias Answer = \"yes\" | \"no\"\n",
        "def render: Answer -> String = value => match value {\n",
        "  \"yes\" => \"affirmative\",\n",
        "  \"no\" => \"negative\",\n",
        "}\n",
    );
    let root = parse(source).expect("string literal types and patterns should parse");
    assert_eq!(root.text(), source);
    let Item::TypeDeclaration(answer) = &root.items[0] else {
        panic!("expected Answer type alias");
    };
    assert!(matches!(answer.underlying, Some(Type::Sum(_))));
    let Statement::Binding(render) = statement(&root.items[1]) else {
        panic!("expected render binding");
    };
    let Some(Expression::Function(function)) = &render.value else {
        panic!("expected render function");
    };
    let Expression::Match(match_) = function.body.as_ref() else {
        panic!("expected match body");
    };
    assert!(
        match_
            .arms
            .iter()
            .all(|arm| matches!(arm.pattern, Pattern::StringLiteral(_)))
    );
}

#[test]
fn parses_traits_implementations_and_bounds_losslessly() {
    let source = concat!(
        "pub trait ToString = T => {\n",
        "  to_string: T -> String\n",
        "}\n",
        "impl ToString I32 {\n",
        "  def to_string = number => \"number\"\n",
        "}\n",
        "def print: T => ToString T => T -> () = value => ()\n",
    );
    let root = parse(source).expect("trait syntax should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        &root.items[0],
        Item::TraitDeclaration(declaration)
            if declaration.visibility == Visibility::Public
                && declaration.name == "ToString"
                && declaration.type_parameters.len() == 1
                && declaration.type_parameters[0].names() == ["T"]
                && declaration.members.len() == 1
    ));
    assert!(matches!(
        &root.items[1],
        Item::TraitImplementation(implementation)
            if implementation.trait_name.name == "ToString"
                && implementation.members.len() == 1
    ));
    let Statement::Binding(binding) = statement(&root.items[2]) else {
        panic!("expected bounded function binding")
    };
    assert_eq!(binding.type_parameters.len(), 1);
    assert_eq!(binding.trait_bounds.len(), 1);
    assert_eq!(binding.trait_bounds[0].trait_name.name, "ToString");
}

#[test]
fn parses_curried_and_product_trait_parameters_and_arguments() {
    let source = concat!(
        "trait Add = Left => Right => Output => { add: (Left, Right) -> Output }\n",
        "trait Convert = (From, To) => { convert: From -> To }\n",
        "impl Add I32 I32 I32 { def add = (left, right) => left + right }\n",
        "impl Convert (I32, String) { def convert = value => \"converted\" }\n",
        "def combine: (L, R, O) => Add L R O => (L, R) -> O = pair => Add.add pair\n",
    );
    let root = parse(source).expect("multi-parameter trait syntax should parse");
    assert_eq!(root.text(), source);

    let Item::TraitDeclaration(add) = &root.items[0] else {
        panic!("expected Add trait");
    };
    assert_eq!(add.type_parameters.len(), 3);
    assert_eq!(add.type_parameters[0].names(), ["Left"]);
    assert_eq!(add.type_parameters[1].names(), ["Right"]);
    assert_eq!(add.type_parameters[2].names(), ["Output"]);

    let Item::TraitDeclaration(convert) = &root.items[1] else {
        panic!("expected Convert trait");
    };
    assert_eq!(convert.type_parameters.len(), 1);
    assert_eq!(convert.type_parameters[0].names(), ["From", "To"]);

    let Item::TraitImplementation(add_impl) = &root.items[2] else {
        panic!("expected Add implementation");
    };
    assert_eq!(add_impl.arguments.len(), 3);
    let Item::TraitImplementation(convert_impl) = &root.items[3] else {
        panic!("expected Convert implementation");
    };
    assert_eq!(convert_impl.arguments.len(), 1);

    let Statement::Binding(combine) = statement(&root.items[4]) else {
        panic!("expected bounded function");
    };
    assert_eq!(combine.trait_bounds[0].arguments.len(), 3);
}

#[test]
fn parses_trait_prerequisites_losslessly() {
    let source = concat!(
        "trait Ord = T => Eq T => { compare: T -> T -> I32 }\n",
        "trait Relation = (Left, Right) => Eq Left => Eq Right => { related: (Left, Right) -> Bool }\n",
    );
    let root = parse(source).expect("trait prerequisites should parse");
    assert_eq!(root.text(), source);

    let Item::TraitDeclaration(ord) = &root.items[0] else {
        panic!("expected Ord trait");
    };
    assert_eq!(ord.prerequisites.len(), 1);
    assert_eq!(ord.prerequisites[0].trait_name.name, "Eq");
    assert_eq!(ord.prerequisites[0].arguments.len(), 1);

    let Item::TraitDeclaration(relation) = &root.items[1] else {
        panic!("expected Relation trait");
    };
    assert_eq!(relation.prerequisites.len(), 2);
    assert!(
        relation
            .prerequisites
            .iter()
            .all(|prerequisite| prerequisite.trait_name.name == "Eq")
    );
}

#[test]
fn parses_trait_functional_dependencies_losslessly() {
    let source = concat!(
        "trait Iterator = Iter => Item => Iter ~> Item => { next: Iter -> Item }\n",
        "trait Add = Left => Right => Output => {Left, Right} ~> Output => Eq Output => { add: Left -> Right -> Output }\n",
        "def iterate: Iter => Iterator Iter => Iter -> () = value => ()\n",
        "def add: T => Add T T _ => T -> T = value => value\n",
    );
    let root = parse(source).expect("functional dependency syntax should parse");
    assert_eq!(root.text(), source);

    let Item::TraitDeclaration(iterator) = &root.items[0] else {
        panic!("expected Iterator trait");
    };
    assert_eq!(iterator.functional_dependencies.len(), 1);
    assert_eq!(
        iterator.functional_dependencies[0].determinants[0].name,
        "Iter"
    );
    assert_eq!(iterator.functional_dependencies[0].dependent.name, "Item");

    let Item::TraitDeclaration(add) = &root.items[1] else {
        panic!("expected Add trait");
    };
    assert_eq!(add.functional_dependencies[0].determinants.len(), 2);
    assert_eq!(add.functional_dependencies[0].dependent.name, "Output");
    assert_eq!(add.prerequisites.len(), 1);

    let Statement::Binding(iterate) = statement(&root.items[2]) else {
        panic!("expected iterate binding");
    };
    assert_eq!(iterate.trait_bounds[0].arguments.len(), 1);
    let Statement::Binding(add_use) = statement(&root.items[3]) else {
        panic!("expected add binding");
    };
    assert_eq!(add_use.trait_bounds[0].arguments.len(), 3);
}

#[test]
fn parses_default_trait_members_losslessly() {
    let source = concat!(
        "trait Increment = T => {\n",
        "  increment: T -> T\n",
        "  twice: T -> T = value => increment (increment value)\n",
        "  identity: T -> T = value => { value }\n",
        "}\n",
    );
    let root = parse(source).expect("default trait members should parse");
    assert_eq!(root.text(), source);
    let Item::TraitDeclaration(trait_) = &root.items[0] else {
        panic!("expected trait declaration");
    };
    assert!(trait_.members[0].default.is_none());
    assert!(trait_.members[1].default.is_some());
    assert!(trait_.members[2].default.is_some());
}

#[test]
fn parses_use_declarations_and_public_items_losslessly() {
    let source = concat!(
        "use path.to.another_module\n",
        "use path.to.another_module *\n",
        "use path.to.another_module (func, MyType)\n",
        "use path.to.another_module func\n",
        "use path.to.another_module func as my_func\n",
        "pub use path.to.another_module PublicType\n",
        "pub type alias PublicType = I32\n",
        "pub def public_value = 1\n",
    );
    let root = parse(source).expect("module syntax should parse");

    assert_eq!(root.text(), source);
    assert!(
        matches!(root.items[0], Item::UseDeclaration(ref use_) if use_.kind == UseKind::Namespace)
    );
    assert!(matches!(root.items[1], Item::UseDeclaration(ref use_) if use_.kind == UseKind::Glob));
    assert!(
        matches!(root.items[2], Item::UseDeclaration(ref use_) if matches!(&use_.kind, UseKind::Selected(names) if names == &["func", "MyType"]))
    );
    assert!(
        matches!(root.items[3], Item::UseDeclaration(ref use_) if matches!(&use_.kind, UseKind::Selected(names) if names == &["func"]))
    );
    assert!(
        matches!(root.items[4], Item::UseDeclaration(ref use_) if matches!(&use_.kind, UseKind::Renamed { item, alias } if item == "func" && alias == "my_func"))
    );
    assert!(
        matches!(root.items[5], Item::UseDeclaration(ref use_) if use_.visibility == Visibility::Public)
    );
    assert!(
        matches!(root.items[6], Item::TypeDeclaration(ref declaration) if declaration.visibility == Visibility::Public)
    );
    let Item::TypeDeclaration(declaration) = &root.items[6] else {
        panic!("expected type declaration")
    };
    assert!(
        declaration
            .syntax
            .text()
            .ends_with("pub type alias PublicType = I32")
    );
    let Statement::Binding(binding) = statement(&root.items[7]) else {
        panic!("expected binding")
    };
    assert_eq!(binding.visibility, Visibility::Public);
    assert!(binding.syntax.text().ends_with("pub def public_value = 1"));
}

#[test]
fn parses_recursive_inline_submodules_losslessly() {
    let source = concat!(
        "pub mod outer {\n",
        "    use super parent\n",
        "    mod inner { pub def value = 42 }\n",
        "}\n",
    );
    let root = parse(source).expect("submodules should parse");
    assert_eq!(root.text(), source);
    let Item::Submodule(outer) = &root.items[0] else {
        panic!("expected outer submodule");
    };
    assert_eq!(outer.name, "outer");
    assert_eq!(outer.visibility, Visibility::Public);
    assert!(matches!(outer.module.items[0], Item::UseDeclaration(_)));
    let Item::Submodule(inner) = &outer.module.items[1] else {
        panic!("expected nested submodule");
    };
    assert_eq!(inner.name, "inner");
    assert_eq!(inner.visibility, Visibility::Private);
}

#[test]
fn parses_namespace_qualified_types() {
    let root = parse("let value: types.Number = 1\n").expect("qualified type should parse");
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding")
    };
    let Some(Type::Named(named)) = &binding.annotation else {
        panic!("expected named type")
    };
    assert_eq!(named.namespace.as_deref(), Some("types"));
    assert_eq!(named.name, "Number");
}

#[test]
fn parses_opaque_type_declarations() {
    let root = parse("pub type I32 = opaque\n").expect("opaque type should parse");
    assert!(matches!(
        root.items[0],
        Item::TypeDeclaration(ref declaration)
            if declaration.kind == TypeDeclarationKind::Opaque
                && declaration.underlying.is_none()
    ));
}

#[test]
fn parses_bodyless_types_as_singletons() {
    let root = parse("pub type Foo\n").expect("singleton type should parse");
    assert!(matches!(
        root.items[0],
        Item::TypeDeclaration(ref declaration)
            if declaration.kind == TypeDeclarationKind::Singleton
                && declaration.underlying.is_none()
    ));
}

#[test]
fn parses_generic_opaque_type_declarations() {
    let root =
        parse("pub type CPointer = Pointee => opaque\n").expect("generic opaque type should parse");
    assert!(matches!(
        root.items[0],
        Item::TypeDeclaration(ref declaration)
            if declaration.kind == TypeDeclarationKind::Opaque
                && declaration.type_parameters.len() == 1
                && declaration.underlying.is_none()
    ));
}

#[test]
fn rejects_removed_pointer_type_syntax() {
    assert!(parse("let pointer: *I32\n").is_err());
    assert!(parse("let pointer: *const I32\n").is_err());
}

#[test]
fn parses_opaque_macro_declarations() {
    let root = parse("pub macro c_string: Expr -> Expr\n").expect("macro declaration should parse");
    assert!(matches!(
        root.items[0],
        Item::MacroDeclaration(ref declaration)
            if declaration.visibility == Visibility::Public
                && declaration.name == "c_string"
                && declaration.annotation.is_some()
                && declaration.value.is_none()
    ));
}

#[test]
fn parses_the_generic_contextual_quote_signature() {
    let source = "pub macro quote: T => QuoteResult T => Braced Syntax -> T\n";
    let root = parse(source).expect("generic primitive macro signature should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        root.items[0],
        Item::MacroDeclaration(ref declaration)
            if declaration.name == "quote"
                && declaration.type_parameters.len() == 1
                && declaration.type_parameters[0].names() == ["T"]
                && declaration.trait_bounds.len() == 1
                && declaration.trait_bounds[0].trait_name.name == "QuoteResult"
                && declaration.annotation.is_some()
    ));
}

#[test]
fn parses_macro_bodies_quotes_and_splices_losslessly() {
    let source = concat!(
        "macro choose = condition => then => else => quote {\n",
        "    match $condition { True() => $then, False() => $else, }\n",
        "}\n",
    );
    let root = parse(source).expect("user macro should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        root.items[0],
        Item::MacroDeclaration(ref declaration)
            if declaration.annotation.is_none() && declaration.value.is_some()
    ));
}

#[test]
fn parses_typed_macro_parameters_and_literal_identifiers() {
    let source = concat!(
        "macro conditional = condition: Expr => then_branch: Expr => ",
        "_: Ident \"else\" => else_branch: Expr => quote { $else_branch }\n",
    );
    let root = parse(source).expect("typed macro parameters should parse");
    assert_eq!(root.text(), source);
    let Item::MacroDeclaration(declaration) = &root.items[0] else {
        panic!("expected macro declaration");
    };
    let Some(Expression::Function(first)) = &declaration.value else {
        panic!("expected first parameter");
    };
    let Expression::Function(second) = first.body.as_ref() else {
        panic!("expected second parameter");
    };
    let Expression::Function(keyword) = second.body.as_ref() else {
        panic!("expected identifier parameter");
    };
    assert!(matches!(
        keyword.pattern,
        Pattern::Wildcard(ref pattern) if !matches!(pattern.ty, Type::Inferred(_))
    ));
}

#[test]
fn reserves_repeated_splice_syntax() {
    let root = parse("macro many = values => quote { $values... }\n")
        .expect("reserved repeated splice should parse");
    let Item::MacroDeclaration(declaration) = &root.items[0] else {
        panic!("expected macro declaration");
    };
    let Some(Expression::Function(function)) = &declaration.value else {
        panic!("expected macro function");
    };
    let Expression::Quote(quote) = function.body.as_ref() else {
        panic!("expected quote");
    };
    assert!(matches!(
        &quote.template,
        stapler::QuoteTemplate::Expression(template)
            if matches!(template.as_ref(), Expression::Splice(splice) if splice.repeated)
    ));
}

#[test]
fn parses_expression_and_single_item_quotations_losslessly() {
    let source = concat!(
        "macro expression = value => quote { $value }\n",
        "macro definition = value => quote { def generated = () => $value; }\n",
        "macro singleton = value => quote { type Generated }\n",
    );
    let root = parse(source).expect("expression and item quotations should parse");
    assert_eq!(root.text(), source);

    let templates = root
        .items
        .iter()
        .map(|item| {
            let Item::MacroDeclaration(declaration) = item else {
                panic!("expected macro declaration");
            };
            let Some(Expression::Function(function)) = &declaration.value else {
                panic!("expected macro function");
            };
            let Expression::Quote(quote) = function.body.as_ref() else {
                panic!("expected quotation");
            };
            &quote.template
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        templates[0],
        stapler::QuoteTemplate::Expression(_)
    ));
    assert!(matches!(
        templates[1],
        stapler::QuoteTemplate::Item(item)
            if matches!(item.as_ref(), Item::Statement(statement)
                if matches!(statement.as_ref(), Statement::Binding(_)))
    ));
    assert!(matches!(
        templates[2],
        stapler::QuoteTemplate::Item(item)
            if matches!(item.as_ref(), Item::TypeDeclaration(_))
    ));

    let module = parse("macro many = _ => quote { def first = 1; def second = 2 }\n")
        .expect("multi-item quotation should parse");
    let Item::MacroDeclaration(declaration) = &module.items[0] else {
        panic!("expected macro declaration");
    };
    let Some(Expression::Function(function)) = &declaration.value else {
        panic!("expected macro function");
    };
    assert!(matches!(
        function.body.as_ref(),
        Expression::Quote(quote) if matches!(&quote.template, stapler::QuoteTemplate::Items(items) if items.len() == 2)
    ));
}

#[test]
fn parses_modifier_macro_definitions_and_item_modifiers_losslessly() {
    let source = concat!(
        "macro @identity: Item -> Item = item => item\n",
        "macro @replace: Expr -> Item -> Item = value => item => quote { let generated = $value }\n",
        "@identity\n",
        "@replace(42)\n",
        "def original = () => 0\n",
    );
    let root = parse(source).expect("modifier macros should parse");
    assert_eq!(root.text(), source);
    let Item::MacroDeclaration(identity) = &root.items[0] else {
        panic!("expected modifier declaration");
    };
    assert!(identity.modifier);
    let Item::Modified(modified) = &root.items[2] else {
        panic!("expected modified item");
    };
    assert_eq!(modified.modifiers.len(), 2);
    assert_eq!(modified.modifiers[0].name, "identity");
    assert!(modified.modifiers[0].argument.is_none());
    assert_eq!(modified.modifiers[1].name, "replace");
    assert!(
        modified.modifiers[1]
            .argument
            .as_ref()
            .is_some_and(|argument| argument.expression.is_some())
    );
    assert!(matches!(modified.item.as_ref(), Item::Statement(_)));
}

#[test]
fn parses_visibility_aware_macro_calls_and_splices_losslessly() {
    let source = concat!(
        "macro define = vis: MacroCallVisibility => ty: Type => quote { $vis type Generated = $ty }\n",
        "pub define I32\n",
        "pub(repr) define I32\n",
        "configure value pub I32\n",
        "configure value pub(repr) I32\n",
    );
    let root = parse(source).expect("visibility syntax should parse");
    assert_eq!(root.text(), source);
    let Item::MacroDeclaration(declaration) = &root.items[0] else {
        panic!("expected macro declaration");
    };
    let Some(Expression::Function(function)) = &declaration.value else {
        panic!("expected macro body");
    };
    let Expression::Function(function) = function.body.as_ref() else {
        panic!("expected second macro parameter");
    };
    let Expression::Quote(quote) = function.body.as_ref() else {
        panic!("expected item quotation");
    };
    assert!(matches!(
        &quote.template,
        stapler::QuoteTemplate::Item(item)
            if matches!(item.as_ref(), Item::VisibilitySplice(_))
    ));
    assert!(matches!(
        &root.items[1],
        Item::VisibilityMacroInvocation(invocation)
            if invocation.visibility.kind == stapler::VisibilityKind::Public
    ));
    assert!(matches!(
        &root.items[2],
        Item::VisibilityMacroInvocation(invocation)
            if invocation.visibility.kind == stapler::VisibilityKind::PublicRepr
    ));
}

#[test]
fn parses_declaration_style_item_macro_punctuation_losslessly() {
    let source = concat!(
        "typegroup Local = { Unit, }\n",
        "pub(repr) typegroup Result = (T, E,) => { Ok T, Err E, }\n",
        "value = 1\n",
        "identity = argument => argument\n",
    );
    let root = parse(source).expect("declaration-style macro calls should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        &root.items[0],
        Item::Statement(statement)
            if matches!(statement.as_ref(), Statement::Expression(Expression::Call(_)))
    ));
    assert!(matches!(
        &root.items[1],
        Item::VisibilityMacroInvocation(invocation)
            if invocation.visibility.kind == stapler::VisibilityKind::PublicRepr
    ));
    assert!(matches!(
        statement(&root.items[2]),
        Statement::Assignment(_)
    ));
    assert!(matches!(
        statement(&root.items[3]),
        Statement::Assignment(_)
    ));
}

#[test]
fn parses_grouped_type_and_pattern_macro_arguments_losslessly() {
    let source = concat!(
        "inspect_type (I32 -> I32)\n",
        "inspect_type (Result I32 Error)\n",
        "inspect_pattern (Some value)\n",
        "inspect_pattern ((left, right)) (40, 2)\n",
    );
    let root = parse(source).expect("grouped category arguments should parse");
    assert_eq!(root.text(), source);
    let Item::Statement(statement) = &root.items[0] else {
        panic!("expected expression statement");
    };
    let Statement::Expression(Expression::Call(call)) = statement.as_ref() else {
        panic!("expected macro call");
    };
    assert!(matches!(
        call.argument.as_ref(),
        Expression::SyntaxArgument(_)
    ));
}

#[test]
fn parses_hello_world_losslessly() {
    let source = include_str!("../examples/hello_world.sta");
    let root = parse(source).expect("hello_world should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 2);
    assert!(matches!(root.items[0], Item::UseDeclaration(_)));
    assert!(matches!(
        root.items[1],
        Item::Statement(ref statement)
            if matches!(statement.as_ref(), Statement::Expression(Expression::Call(_)))
    ));
}

#[test]
fn parses_product_parameter_and_expression_body() {
    let source = "def add: _ -> I32 = (a: I32, b: I32) => a + b\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Expression::Function(function) = binding.value.as_ref().expect("function value") else {
        panic!("expected function");
    };
    assert!(
        matches!(function.pattern, Pattern::Product(ref product) if product.elements.len() == 2)
    );
    let Expression::Call(outer) = function.body.as_ref() else {
        panic!("expected desugared `+` call");
    };
    let Expression::Call(inner) = outer.callee.as_ref() else {
        panic!("expected desugared `+` call");
    };
    let Expression::Access(access) = inner.callee.as_ref() else {
        panic!("expected `Add.add` access");
    };
    assert!(matches!(access.value.as_ref(), Expression::Name(name) if name.name == "Add"));
    assert!(matches!(&access.accessor, Accessor::Name(name) if name == "add"));
    assert!(matches!(inner.argument.as_ref(), Expression::Name(name) if name.name == "a"));
    assert!(matches!(outer.argument.as_ref(), Expression::Name(name) if name.name == "b"));
}

#[test]
fn parses_low_precedence_satisfies_expression() {
    let source = "let add = (a: I32, b: I32) => a + b satisfies I32\n";
    let root = parse(source).expect("function should parse");
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };

    let Expression::Satisfies(satisfies) = function.body.as_ref() else {
        panic!("expected satisfies expression");
    };
    assert!(matches!(satisfies.ty, Type::Named(ref named) if named.name == "I32"));
    assert!(matches!(satisfies.value.as_ref(), Expression::Call(_)));
    assert_eq!(root.text(), source);
}

#[test]
fn rejects_removed_inline_function_result_annotation() {
    assert!(parse("let add = (a: I32, b: I32) -> I32 => a + b\n").is_err());
}

#[test]
fn parses_contextually_typed_curried_parameters() {
    let source = "def add: I32 -> I32 -> I32 = a => b => a + b\n";
    let root = parse(source).expect("curried function should parse");
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(outer)) = &binding.value else {
        panic!("expected outer function");
    };

    assert!(matches!(outer.pattern.ty(), Type::Inferred(_)));
    assert!(matches!(outer.body.as_ref(), Expression::Function(_)));
    assert_eq!(root.text(), source);
}

#[test]
fn parses_mixed_precedence_builtin_operator_expression() {
    let source = "1 + 2 * 3\n";
    let root = parse(source).expect("builtin operator expression should parse");
    let Statement::Expression(Expression::Call(outer)) = statement(&root.items[0]) else {
        panic!("expected desugared `+` call");
    };
    let Expression::Call(add_call) = outer.callee.as_ref() else {
        panic!("expected desugared `+` call");
    };
    let Expression::Access(access) = add_call.callee.as_ref() else {
        panic!("expected `Add.add` access");
    };
    assert!(matches!(access.value.as_ref(), Expression::Name(name) if name.name == "Add"));
    assert!(matches!(&access.accessor, Accessor::Name(name) if name == "add"));
    assert!(matches!(add_call.argument.as_ref(), Expression::Integer(int) if int.literal == "1"));

    let Expression::Call(multiply_outer) = outer.argument.as_ref() else {
        panic!("expected `2 * 3` nested under `+`");
    };
    let Expression::Call(multiply_call) = multiply_outer.callee.as_ref() else {
        panic!("expected desugared `*` call");
    };
    let Expression::Access(multiply_access) = multiply_call.callee.as_ref() else {
        panic!("expected `Multiply.multiply` access");
    };
    assert!(
        matches!(multiply_access.value.as_ref(), Expression::Name(name) if name.name == "Multiply")
    );
    assert!(matches!(&multiply_access.accessor, Accessor::Name(name) if name == "multiply"));
    assert_eq!(root.text(), source);
}

#[test]
fn rejects_chained_comparison_operators() {
    let error = parse("1 == 2 == 3\n").expect_err("chained comparisons should be rejected");
    assert!(error.message.contains("cannot be chained"));
}

#[test]
fn rejects_chained_range_operators() {
    let error = parse("0 .. 1 .. 2\n").expect_err("chained ranges should be rejected");
    assert!(error.message.contains("cannot be chained"));
}

#[test]
fn rejects_dot_as_a_binding_name() {
    let error = parse("def .: I32 -> I32 = x => x\n")
        .expect_err("dot should remain punctuation rather than a binding name");

    assert_eq!(error.message, "expected binding name");
}

#[test]
fn rejects_the_old_function_body_arrow() {
    let error = parse("let answer = () -> 42\n").expect_err("old syntax should not parse");

    assert!(error.message.contains("expected `=>`"));
}

#[test]
fn parse_errors_report_one_based_line_and_character_column() {
    let error =
        parse("let first = 1\nlet café = ").expect_err("missing expression should not parse");

    assert_eq!(
        error.to_string(),
        "expected expression at line 2, column 12"
    );
}

#[test]
fn parses_single_parameter_and_application() {
    let source = "def identity: _ -> String = (s: String) => consume (s)\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Expression::Function(function) = binding.value.as_ref().expect("function value") else {
        panic!("expected function");
    };
    assert!(matches!(
        function.pattern,
        Pattern::Product(ref product)
            if matches!(
                product.elements.first(),
                Some(Pattern::Binding(binding)) if binding.name == "s"
            )
    ));
    let Expression::Call(call) = function.body.as_ref() else {
        panic!("expected call");
    };
    assert!(matches!(*call.argument, Expression::Product(_)));
}

#[test]
fn parses_nested_product_patterns_losslessly() {
    let source = "let first = (x: I32, (y: I32, z: I32)) => x + y\n";
    let root = parse(source).expect("nested product pattern should parse");
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };
    let Pattern::Product(pattern) = &function.pattern else {
        panic!("expected product pattern");
    };

    assert!(matches!(pattern.elements[1], Pattern::Product(_)));
    assert_eq!(root.text(), source);
}

#[test]
fn parses_at_patterns_losslessly_and_right_associatively() {
    let source = concat!(
        "let point@(x, y) = (1, 2)\n",
        "let mut outer: (I32, I32)@inner@(left, right) = (1, 2)\n",
        "def copy = var outer@(left, right) => outer\n",
    );
    let root = parse(source).expect("at-patterns should parse");
    assert_eq!(root.text(), source);

    let Statement::PatternBinding(point) = statement(&root.items[0]) else {
        panic!("expected destructuring binding");
    };
    assert!(matches!(point.pattern, Pattern::At(_)));

    let Statement::PatternBinding(outer_let) = statement(&root.items[1]) else {
        panic!("expected destructuring binding");
    };
    let Pattern::At(outer) = &outer_let.pattern else {
        panic!("expected outer at-pattern");
    };
    assert!(outer.binding.mutable);
    assert!(matches!(outer.pattern.as_ref(), Pattern::At(_)));

    let Statement::Binding(copy) = statement(&root.items[2]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &copy.value else {
        panic!("expected function");
    };
    let Pattern::At(parameter) = &function.pattern else {
        panic!("expected parameter at-pattern");
    };
    assert!(parameter.binding.reassignable);

    assert!(parse("let _@(x, y) = (1, 2)\n").is_err());
    assert!(parse("let (x, y)@point = (1, 2)\n").is_err());
    assert!(parse("let point@ = (1, 2)\n").is_err());
    assert!(parse("def bad = mut outer@(left, right) => outer\n").is_err());
}

#[test]
fn comments_and_crlf_are_preserved() {
    let source = "// entry\r\ndef main: _ -> String = () => {\r\n  \"ok\"\r\n}\r\n";
    let root = parse(source).expect("source should parse");
    assert_eq!(root.text(), source);
    assert!(
        root.syntax
            .tokens()
            .iter()
            .any(|token| token.kind == TokenKind::LineComment)
    );
    assert!(
        root.syntax
            .tokens()
            .iter()
            .any(|token| token.text == "\r\n")
    );
}

#[test]
fn parses_named_product_types_values_and_access() {
    let source = concat!(
        "let args: (name: String, int)\n",
        "type user_id = int\n",
        "def value = (name: \"staple\", 1)\n",
        "def by_name = args.name\n",
        "def by_index = args.1\n",
    );
    let root = parse(source).expect("named product syntax should parse");

    assert_eq!(root.text(), source);
    let Statement::Binding(args) = statement(&root.items[0]) else {
        panic!("expected args binding");
    };
    let Type::Product(args_type) = args.annotation.as_ref().expect("args type") else {
        panic!("expected product type");
    };
    assert_eq!(args_type.elements[0].name.as_deref(), Some("name"));
    assert_eq!(args_type.elements[1].name, None);

    assert!(matches!(
        root.items[1],
        Item::TypeDeclaration(ref declaration)
            if declaration.kind == TypeDeclarationKind::Distinct
    ));

    let Statement::Binding(value) = statement(&root.items[2]) else {
        panic!("expected value binding");
    };
    let Expression::Product(value) = value.value.as_ref().expect("product value") else {
        panic!("expected product value");
    };
    assert_eq!(value.elements[0].name.as_deref(), Some("name"));

    let Statement::Binding(by_name) = statement(&root.items[3]) else {
        panic!("expected name access binding");
    };
    assert!(matches!(
        by_name.value,
        Some(Expression::Access(ref access)) if access.accessor == Accessor::Name("name".into())
    ));

    let Statement::Binding(by_index) = statement(&root.items[4]) else {
        panic!("expected index access binding");
    };
    assert!(matches!(
        by_index.value,
        Some(Expression::Access(ref access)) if access.accessor == Accessor::Index("1".into())
    ));
}

#[test]
fn type_declaration_underlying_type_stops_at_newline() {
    let source = concat!(
        "type Test = ()\n",
        "\n",
        "std.io.println \"Hello, world!\"\n",
    );
    let root = parse(source).expect("statement after a type alias should parse");
    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 2);

    assert!(matches!(
        root.items[0],
        Item::TypeDeclaration(ref declaration)
            if matches!(
                declaration.underlying,
                Some(Type::Product(ref product)) if product.elements.is_empty()
            )
    ));

    let Item::Statement(statement) = &root.items[1] else {
        panic!("expected expression statement");
    };
    let Statement::Expression(Expression::Call(call)) = statement.as_ref() else {
        panic!("expected a call expression, not a continued type application");
    };
    let Expression::Access(println) = call.callee.as_ref() else {
        panic!("expected `std.io.println` access chain as callee");
    };
    assert_eq!(println.accessor, Accessor::Name("println".into()));
}

#[test]
fn parses_compile_time_parameters_and_type_application() {
    let source = concat!(
        "type alias Pair = (A, B) => (A, B)\n",
        "type Box = T => (value: T)\n",
        "def identity: T => T -> T = x => x\n",
        "let pair: Pair (String, I32)\n",
    );
    let root = parse(source).expect("generic syntax should parse");
    assert_eq!(root.text(), source);
    let Item::TypeDeclaration(pair) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert_eq!(pair.type_parameters.len(), 1);
    let Statement::Binding(identity) = statement(&root.items[2]) else {
        panic!("expected generic function binding");
    };
    assert_eq!(identity.type_parameters.len(), 1);
    let Statement::Binding(value) = statement(&root.items[3]) else {
        panic!("expected annotated binding");
    };
    assert!(matches!(value.annotation, Some(Type::Application(_))));
}

#[test]
fn parses_sized_relaxations_losslessly() {
    let source = concat!(
        "pub(repr) type RefLike = T => ?Sized T => T\n",
        "def preserve: T => ?Sized T => Ref T -> Ref T = value => value\n",
        "def ordinary: T => T -> T = value => value\n",
    );
    let root = parse(source).expect("`?Sized` clauses should parse");
    assert_eq!(root.text(), source);

    let Item::TypeDeclaration(reference) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert!(matches!(
        reference.type_parameters.as_slice(),
        [stapler::TypeParameterPattern::Binding(binding)] if !binding.sized
    ));

    let Statement::Binding(preserve) = statement(&root.items[1]) else {
        panic!("expected generic binding");
    };
    assert!(matches!(
        preserve.type_parameters.as_slice(),
        [stapler::TypeParameterPattern::Binding(binding)] if !binding.sized
    ));

    let Statement::Binding(ordinary) = statement(&root.items[2]) else {
        panic!("expected generic binding");
    };
    assert!(matches!(
        ordinary.type_parameters.as_slice(),
        [stapler::TypeParameterPattern::Binding(binding)] if binding.sized
    ));

    assert!(parse("type alias Bad = T => ?Other T => T\n").is_err());
    assert!(parse("type alias Bad = T => ?Sized U => T\n").is_err());
    assert!(parse("type alias Bad = T => ?Sized T => ?Sized T => T\n").is_err());
}

#[test]
fn parses_public_representations_and_nominal_patterns() {
    let source = concat!(
        "pub(repr) type Box = T => (value: T)\n",
        "let Box (value) = Box (value: 42)\n",
        "def unbox: Box I32 -> I32 = Box value => value\n",
    );
    let root = parse(source).expect("nominal patterns should parse");
    let Item::TypeDeclaration(declaration) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert_eq!(declaration.representation_visibility, Visibility::Public);
    assert!(matches!(
        statement(&root.items[1]),
        Statement::PatternBinding(binding)
            if matches!(binding.pattern, Pattern::Nominal(_))
    ));
    let Statement::Binding(binding) = statement(&root.items[2]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function expression");
    };
    assert!(matches!(function.pattern, Pattern::Nominal(_)));
}

#[test]
fn rejects_invalid_public_representation_and_pattern_visibility_syntax() {
    assert!(parse("pub(repr) type alias Number = I32\n").is_err());
    assert!(parse("pub(repr) type Handle = opaque\n").is_err());
    assert!(parse("pub(repr) type Singleton\n").is_ok());
    assert!(parse("pub let (a, b) = (1, 2)\n").is_err());
    assert!(parse("pub(repr) def value = 1\n").is_err());
}

#[test]
fn block_items_are_typed() {
    let source = "def answer = () => { let x = 40 }\n";
    let root = parse(source).expect("block should parse");
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected block");
    };
    assert!(matches!(block.statements[0], Statement::Binding(_)));
}

#[test]
fn parses_multiple_top_level_statements() {
    let source = "let greeting = \"hello\"\nprintln greeting\nprintln \"second\"\n";
    let root = parse(source).expect("top-level statements should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 3);
    assert!(matches!(
        root.items[0],
        Item::Statement(ref statement) if matches!(statement.as_ref(), Statement::Binding(_))
    ));
    assert!(matches!(root.items[1], Item::Statement(_)));
    assert!(matches!(root.items[2], Item::Statement(_)));
}

#[test]
fn parses_returns_and_semicolon_separated_items_losslessly() {
    let source = concat!(
        "use std.core *;",
        "type alias Number = I32;",
        "extern \"c\" { let exit: I32 -> (); };",
        "def answer = () => { let value = 42; return value; };",
        "answer ();",
    );
    let root = parse(source).expect("semicolon-separated source should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 5);
    let Statement::Binding(answer) = statement(&root.items[3]) else {
        panic!("expected answer binding");
    };
    let Some(Expression::Function(function)) = &answer.value else {
        panic!("expected function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected function block");
    };
    assert!(matches!(block.statements[1], Statement::Return(_)));
    assert!(
        root.syntax
            .tokens()
            .iter()
            .any(|token| token.kind == TokenKind::Return)
    );
    assert!(
        root.syntax
            .tokens()
            .iter()
            .any(|token| token.kind == TokenKind::Semicolon)
    );
}

#[test]
fn rejects_missing_return_values_and_empty_separators() {
    assert!(parse("def invalid = () => { return; }\n").is_err());
    assert!(parse("def invalid = () => { return\n42 }\n").is_err());
    assert!(parse("let value = 1;; value\n").is_err());
}

#[test]
fn parses_sum_types_and_propagating_patterns_losslessly() {
    let source = concat!(
        "def read: String -> Ok String | IOError = path => Ok(path)\n",
        "def parse = (path: String) => { let Ok(file)? = read(path); Ok(file) }\n",
    );
    let root = parse(source).expect("sum and propagation syntax should parse");
    assert_eq!(root.text(), source);
    let Statement::Binding(read) = statement(&root.items[0]) else {
        panic!("expected read binding");
    };
    assert!(
        matches!(read.annotation, Some(Type::Function(ref function)) if matches!(function.result.as_ref(), Type::Sum(_)))
    );
    let Statement::Binding(parse_binding) = statement(&root.items[1]) else {
        panic!("expected parse binding");
    };
    let Some(Expression::Function(function)) = &parse_binding.value else {
        panic!("expected function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected block");
    };
    assert!(matches!(
        block.statements[0],
        Statement::PatternBinding(ref binding)
            if binding.kind == stapler::PatternBindingKind::Propagating
    ));
}

#[test]
fn parses_repeated_spread_erased_and_variable_index_syntax() {
    let source = concat!(
        "let fixed: Ref I32[3]\n",
        "let erased: Ref I32[]\n",
        "let mixed: (String, ...I32[3], ...(I32, I32))\n",
        "let expanded = (prefix: \"value\", ...mixed, suffix: 1)\n",
        "let value = erased[index].0\n",
    );
    let module = parse(source).expect("product extensions should parse");
    assert_eq!(module.text(), source);

    let Statement::Binding(fixed) = statement(&module.items[0]) else {
        panic!("expected fixed binding");
    };
    assert!(matches!(fixed.annotation, Some(Type::Application(_))));

    let Statement::Binding(expanded) = statement(&module.items[3]) else {
        panic!("expected expanded binding");
    };
    let Some(Expression::Product(expanded)) = &expanded.value else {
        panic!("expected expanded product value");
    };
    assert!(expanded.elements[1].spread);

    let Statement::Binding(value) = statement(&module.items[4]) else {
        panic!("expected indexed binding");
    };
    assert!(matches!(value.value, Some(Expression::Access(_))));
}

#[test]
fn parses_named_product_value_spread_syntax() {
    let source = concat!(
        "let dimensions = (height: 600, width: 800)\n",
        "let config: (width: I32, height: I32, title: String) = (\n",
        "    ...=dimensions,\n",
        "    title: \"Staple\",\n",
        ")\n",
    );
    let module = parse(source).expect("named product spreads should parse");
    assert_eq!(module.text(), source);

    let Statement::Binding(config) = statement(&module.items[1]) else {
        panic!("expected config binding");
    };
    let Some(Expression::Product(config)) = &config.value else {
        panic!("expected config product value");
    };
    assert!(config.elements[0].spread);
    assert!(config.elements[0].named_spread);
    assert!(config.elements[0].name.is_none());
    assert!(!config.elements[1].spread);
    assert!(!config.elements[1].named_spread);
}

#[test]
fn rejects_a_named_element_before_a_named_product_spread() {
    let source = "let value = (name: ...=other)\n";
    let error = parse(source).expect_err("a named element before a spread should be rejected");
    assert!(error.message.contains("cannot be named"));
}

#[test]
fn parses_mutable_patterns_and_assignment_statements_losslessly() {
    let source = concat!(
        "let mut value = 1\n",
        "let (mut left, right) = (2, 3)\n",
        "def update = (var parameter: I32) => { parameter = 4; parameter }\n",
        "value = left\n",
    );
    let module = parse(source).expect("mutable bindings and assignments should parse");
    assert_eq!(module.text(), source);
    let Statement::Binding(value) = statement(&module.items[0]) else {
        panic!("expected mutable binding");
    };
    assert!(value.mutable);
    let Statement::PatternBinding(pair) = statement(&module.items[1]) else {
        panic!("expected pattern binding");
    };
    let Pattern::Product(pattern) = &pair.pattern else {
        panic!("expected product pattern");
    };
    assert!(matches!(&pattern.elements[0], Pattern::Binding(binding) if binding.mutable));
    assert!(matches!(
        statement(&module.items[3]),
        Statement::Assignment(_)
    ));
    assert!(parse("extern \"c\" { let mut value: I32 }\n").is_err());
    assert!(parse("def bad = (mut parameter: I32) => parameter\n").is_err());
}

#[test]
fn parses_var_bindings_and_pattern_propagation_losslessly() {
    let source = concat!(
        "var value = 1\n",
        "var mut counter = 2\n",
        "var (left, right) = (3, 4)\n",
        "let (var alpha, mut beta) = (5, 6)\n",
        "def update = (var parameter: I32) => { parameter = 7; parameter }\n",
        "match update 0 { Box (var inner) => inner, _ => 0 }\n",
        "var outer@(nested_left, nested_right) = (8, 9)\n",
        "value = right\n",
    );
    let module = parse(source).expect("var bindings should parse");
    assert_eq!(module.text(), source);

    let Statement::Binding(value) = statement(&module.items[0]) else {
        panic!("expected var binding");
    };
    assert!(value.reassignable && !value.mutable);

    let Statement::Binding(counter) = statement(&module.items[1]) else {
        panic!("expected var mut binding");
    };
    assert!(counter.reassignable && counter.mutable);

    let Statement::PatternBinding(pair) = statement(&module.items[2]) else {
        panic!("expected propagated var pattern binding");
    };
    let Pattern::Product(pattern) = &pair.pattern else {
        panic!("expected product pattern");
    };
    assert!(pattern.elements.iter().all(|element| matches!(
        element,
        Pattern::Binding(binding) if binding.reassignable && !binding.mutable
    )));

    let Statement::PatternBinding(mixed) = statement(&module.items[3]) else {
        panic!("expected per-name pattern binding");
    };
    let Pattern::Product(mixed_pattern) = &mixed.pattern else {
        panic!("expected product pattern");
    };
    assert!(matches!(
        &mixed_pattern.elements[0],
        Pattern::Binding(binding) if binding.reassignable && !binding.mutable
    ));
    assert!(matches!(
        &mixed_pattern.elements[1],
        Pattern::Binding(binding) if binding.mutable && !binding.reassignable
    ));

    let Statement::Binding(update) = statement(&module.items[4]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &update.value else {
        panic!("expected function value");
    };
    let Pattern::Product(parameters) = &function.pattern else {
        panic!("expected product pattern");
    };
    assert!(matches!(
        &parameters.elements[0],
        Pattern::Binding(binding) if binding.reassignable && !binding.mutable
    ));

    let Statement::PatternBinding(outer) = statement(&module.items[6]) else {
        panic!("expected at-pattern var propagation");
    };
    let Pattern::At(at) = &outer.pattern else {
        panic!("expected at-pattern");
    };
    assert!(at.binding.reassignable);
    let Pattern::Product(nested) = at.pattern.as_ref() else {
        panic!("expected nested product pattern");
    };
    assert!(nested.elements.iter().all(|element| matches!(
        element,
        Pattern::Binding(binding) if binding.reassignable
    )));

    assert!(parse("def var value = 1\n").is_err());
    assert!(parse("extern \"c\" { var value: I32 }\n").is_err());
    assert!(parse("mut var value = 1\n").is_err());
    assert!(parse("let (var Box inner, extra) = pair\n").is_err());
}

#[test]
fn parses_loop_break_and_continue_losslessly() {
    let source = concat!(
        "def choose = () => loop {\n",
        "  loop { break }\n",
        "  break 42\n",
        "  continue;\n",
        "}\n",
    );
    let module = parse(source).expect("loop control should parse");
    assert_eq!(module.text(), source);
    let Statement::Binding(binding) = statement(&module.items[0]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function expression");
    };
    let Expression::Loop(loop_) = function.body.as_ref() else {
        panic!("expected loop expression");
    };
    assert!(matches!(
        loop_.body.statements[0],
        Statement::Expression(Expression::Loop(_))
    ));
    assert!(matches!(
        loop_.body.statements[1],
        Statement::Break(ref break_) if break_.value.is_some()
    ));
    assert!(matches!(loop_.body.statements[2], Statement::Continue(_)));
    assert!(
        module
            .syntax
            .tokens()
            .iter()
            .any(|token| token.kind == TokenKind::Loop)
    );
    assert!(
        module
            .syntax
            .tokens()
            .iter()
            .any(|token| token.kind == TokenKind::Break)
    );
    assert!(
        module
            .syntax
            .tokens()
            .iter()
            .any(|token| token.kind == TokenKind::Continue)
    );
}

#[test]
fn uses_newline_as_the_unit_break_boundary() {
    let module = parse("def value = () => loop { break\n42 }\n")
        .expect("newline should terminate a unit break");
    let Statement::Binding(binding) = statement(&module.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };
    let Expression::Loop(loop_) = function.body.as_ref() else {
        panic!("expected loop");
    };
    assert!(matches!(
        loop_.body.statements[0],
        Statement::Break(ref break_) if break_.value.is_none()
    ));
    assert!(matches!(loop_.body.statements[1], Statement::Expression(_)));
    assert!(parse("def invalid = () => loop { continue 42 }\n").is_err());
}
