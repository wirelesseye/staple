use stapler::{
    Accessor, BindingKind, Expression, Item, LogicalOperator, Pattern, StringInterpolationFormat,
    StringTemplatePart, TokenKind, Type, TypeDeclarationKind, UseKind, Visibility, parse,
};

#[test]
fn parses_string_templates_and_preserves_source() {
    let source = "let name = \"world\"\nlet message = \"hello $name: ${1 + 2}; ${name:?}; \\$5; ${\"nested\"}\"\n";
    let module = parse(source).expect("string templates should parse");
    assert_eq!(module.syntax.text(), source);
    let Item::Binding(binding) = &module.items[1] else {
        panic!("expected template binding")
    };
    let Some(Expression::StringTemplate(template)) = &binding.value else {
        panic!("expected string template")
    };
    assert_eq!(template.parts.len(), 8);
    assert!(matches!(
        &template.parts[1],
        StringTemplatePart::Interpolation(value)
            if value.format == StringInterpolationFormat::Display
                && matches!(value.expression.as_ref(), Expression::Name(name) if name.name == "name")
    ));
    assert!(matches!(
        &template.parts[5],
        StringTemplatePart::Interpolation(value)
            if value.format == StringInterpolationFormat::Debug
    ));
    assert!(matches!(&template.parts[6], StringTemplatePart::Literal(value) if value == "; $5; "));
    assert!(matches!(
        &template.parts[7],
        StringTemplatePart::Interpolation(value)
            if matches!(value.expression.as_ref(), Expression::String(_))
    ));

    assert!(parse("let invalid = \"$\"\n").is_err());
    assert!(parse("let invalid = \"${}\"\n").is_err());
    assert!(parse("let invalid = \"${1\"\n").is_err());
}

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
    let item = &module.items[1];
    let Item::Binding(binding) = item else {
        panic!("expected binding")
    };
    let Some(Type::Function(function)) = &binding.annotation else {
        panic!("expected function annotation")
    };
    assert_eq!(function.effects.resources.len(), 1);
    assert!(matches!(module.items[3], Item::Expression(_)));
    assert!(matches!(module.items[4], Item::MacroDeclaration(_)));
    assert!(matches!(module.items[5], Item::MacroDeclaration(_)));

    assert!(parse("resource () ->\n").is_err());
    assert!(parse("with Clock = {}\n").is_err());
    assert!(parse("def bad: () ->{Clock I32 = () => 0\n").is_err());
}

#[test]
fn parses_signal_bindings_losslessly() {
    let source = "let signal count: I32 = 0\n";
    let module = parse(source).expect("signal binding should parse");
    assert_eq!(module.syntax.text(), source);
    assert!(matches!(&module.items[0], Item::Binding(binding)
        if binding.signal && !binding.mutable && binding.name == "count"));

    assert!(parse("let signal missing\n").is_err());
    assert!(parse("let mut signal duplicate = 0\n").is_err());
    assert!(parse("def signal invalid = 0\n").is_err());
}

#[test]
fn parses_effect_parameters_and_open_effect_rows() {
    let source = "def twice: <T, effect E> (T, () ->{E} ()) ->{E, IO} T = (value, f) => value\n";
    let module = parse(source).expect("effect parameters should parse");
    assert_eq!(module.syntax.text(), source);
    let Item::Binding(binding) = &module.items[0] else {
        panic!("expected binding")
    };
    assert!(
        matches!(binding.type_parameters.as_slice(), [stapler::TypeParameterPattern::Binding(_), stapler::TypeParameterPattern::Effect(value)] if value.name == "E")
    );
}

#[test]
fn parses_fully_qualified_quote_expressions_losslessly() {
    let source = concat!(
        "macro capture = value => std.syntax.quote { $value }\n",
        "macro parsed = value => std.syntax.parse_quote { $value }\n",
        "macro imported = value => syntax.parse_quote { $value }\n",
    );
    let module = parse(source).expect("qualified quotations should parse");
    assert_eq!(module.syntax.text(), source);
}

#[test]
fn parses_mutable_parameter_types_losslessly() {
    use stapler::{MutationTarget, MutationTargetKind};

    fn mutations(annotation: &Option<Type>) -> &[MutationTarget] {
        let Some(Type::Function(function)) = annotation else {
            panic!("expected function annotation");
        };
        &function.mutations
    }

    let source = concat!(
        "def f1: mut A -> () = a => ()\n",
        "def f2: (mut A, B) -> () = (a, b) => ()\n",
        "def f3: (mut a: A, b: B) -> () = (a, b) => ()\n",
        "def f4: (mut a: A, b: B) ->{IO} () = (a, b) => ()\n",
        "let operation: mut A -> () = f1\n",
    );
    let module = parse(source).expect("mutable parameter types should parse");
    assert_eq!(module.text(), source);

    let Item::Binding(f1) = unmodified_item(&module.items[0]) else {
        panic!("expected binding");
    };
    assert!(matches!(
        mutations(&f1.annotation),
        [MutationTarget {
            target: MutationTargetKind::Whole,
            ..
        }]
    ));

    let Item::Binding(f2) = unmodified_item(&module.items[1]) else {
        panic!("expected binding");
    };
    assert!(matches!(
        mutations(&f2.annotation),
        [MutationTarget {
            target: MutationTargetKind::Element(0),
            ..
        }]
    ));

    let Item::Binding(f3) = unmodified_item(&module.items[2]) else {
        panic!("expected binding");
    };
    assert!(matches!(
        mutations(&f3.annotation),
        [MutationTarget {
            target: MutationTargetKind::Element(0),
            ..
        }]
    ));

    let Item::Binding(f4) = unmodified_item(&module.items[3]) else {
        panic!("expected binding");
    };
    let Some(Type::Function(function)) = &f4.annotation else {
        panic!("expected function annotation");
    };
    assert_eq!(function.mutations.len(), 1);
    assert_eq!(function.effects.resources.len(), 1);

    let error = parse("def bad: A ->{mut} () = a => ()\n").expect_err("legacy syntax must fail");
    assert!(error.message.contains("`mut` is not an effect"));
    assert!(parse("def bad: (a: mut A, b: B) -> () = (a, b) => ()\n").is_err());
    assert!(parse("def bad: ((mut A, B), C) -> () = (a, b) => ()\n").is_err());
    assert!(parse("def bad: mut (mut A, B) -> () = (a, b) => ()\n").is_err());
}

fn unmodified_item(item: &Item) -> &Item {
    item
}

#[test]
fn parses_float_literals_losslessly_without_stealing_access_dots() {
    let source = "1.0\n1.\n.5\n1e3\n1.5e-2\n1.e2\n1.field\n(1, 2).0\n";
    let root = parse(source).expect("float literals should parse");
    assert_eq!(root.text(), source);
    for item in &root.items[..6] {
        assert!(matches!(
            unmodified_item(item),
            Item::Expression(Expression::Float(_))
        ));
    }
    assert!(matches!(
        unmodified_item(&root.items[6]),
        Item::Expression(Expression::Access(access))
            if matches!(access.value.as_ref(), Expression::Integer(_))
                && access.accessor == Accessor::Name("field".into())
    ));
    assert!(matches!(
        unmodified_item(&root.items[7]),
        Item::Expression(Expression::Access(access))
            if access.accessor == Accessor::Index("0".into())
    ));
    assert_eq!(
        stapler::lex("1e+")[0].kind,
        TokenKind::Float,
        "an incomplete exponent should remain one diagnostic token"
    );
}

#[test]
fn lexes_only_the_fixed_operator_vocabulary() {
    let source = "..= && || == != <= >= .. <: ~> ? | < > ^ *^ *. ++ %";
    let tokens = stapler::lex(source)
        .into_iter()
        .filter(|token| !token.kind.is_trivia())
        .map(|token| (token.kind, token.text))
        .collect::<Vec<_>>();
    assert_eq!(
        tokens,
        [
            (TokenKind::Operator, "..="),
            (TokenKind::Operator, "&&"),
            (TokenKind::Operator, "||"),
            (TokenKind::Operator, "=="),
            (TokenKind::Operator, "!="),
            (TokenKind::Operator, "<="),
            (TokenKind::Operator, ">="),
            (TokenKind::Operator, ".."),
            (TokenKind::Operator, "<:"),
            (TokenKind::Operator, "~>"),
            (TokenKind::Operator, "?"),
            (TokenKind::Operator, "|"),
            (TokenKind::Operator, "<"),
            (TokenKind::Operator, ">"),
            (TokenKind::Operator, "^"),
            (TokenKind::Star, "*"),
            (TokenKind::Operator, "^"),
            (TokenKind::Star, "*"),
            (TokenKind::Dot, "."),
            (TokenKind::Plus, "+"),
            (TokenKind::Plus, "+"),
            (TokenKind::Unknown, "%"),
        ]
        .map(|(kind, text)| (kind, text.to_owned()))
    );
}

#[test]
fn parses_nominal_representation_access_losslessly() {
    let source = "user.*\nuser.*.name\nouter.*.*.0\nlist.*^length\n";
    let root = parse(source).expect("representation access should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        unmodified_item(&root.items[0]),
        Item::Expression(Expression::Access(access))
            if access.accessor == Accessor::Representation
    ));
    assert!(matches!(
        unmodified_item(&root.items[1]),
        Item::Expression(Expression::Access(access))
            if access.accessor == Accessor::Name("name".into())
                && matches!(access.value.as_ref(), Expression::Access(inner) if inner.accessor == Accessor::Representation)
    ));
    let Item::Expression(Expression::Access(index)) = unmodified_item(&root.items[2]) else {
        panic!("expected positional access after representation projections");
    };
    assert_eq!(index.accessor, Accessor::Index("0".into()));
    let Expression::Access(middle) = index.value.as_ref() else {
        panic!("expected second representation projection");
    };
    assert_eq!(middle.accessor, Accessor::Representation);
    let Expression::Access(inner) = middle.value.as_ref() else {
        panic!("expected first representation projection");
    };
    assert_eq!(inner.accessor, Accessor::Representation);
    let Item::Expression(Expression::Call(call)) = unmodified_item(&root.items[3]) else {
        panic!("expected a companion call after representation access");
    };
    assert!(matches!(
        call.callee.as_ref(),
        Expression::Access(method)
            if method.accessor == Accessor::Method("length".into())
                && matches!(method.value.as_ref(), Expression::Access(representation)
                    if representation.accessor == Accessor::Representation)
    ));
    assert!(parse("user.+\n").is_err());
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
    let Item::Binding(choose) = unmodified_item(&root.items[0]) else {
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

    let Item::Binding(discard) = unmodified_item(&root.items[1]) else {
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
    let Item::Binding(discard) = unmodified_item(&root.items[0]) else {
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
    let Item::Binding(render) = unmodified_item(&root.items[1]) else {
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
        "pub trait ToString T {\n",
        "  to_string: T -> String\n",
        "}\n",
        "impl ToString I32 {\n",
        "  def to_string = number => \"number\"\n",
        "}\n",
        "def print: <T where ToString T> T -> () = value => ()\n",
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
    let Item::Binding(binding) = unmodified_item(&root.items[2]) else {
        panic!("expected bounded function binding")
    };
    assert_eq!(binding.type_parameters.len(), 1);
    assert_eq!(binding.trait_bounds.len(), 1);
    assert_eq!(binding.trait_bounds[0].trait_name.name, "ToString");
}

#[test]
fn parses_negative_trait_implementations_losslessly() {
    let source = concat!("type Handle = I32\n", "impl !Copy Handle {}\n",);
    let root = parse(source).expect("negative impl syntax should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        &root.items[1],
        Item::TraitImplementation(implementation)
            if implementation.negative
                && implementation.trait_name.name == "Copy"
                && implementation.members.is_empty()
    ));
}

#[test]
fn parses_generic_conditional_trait_implementations_losslessly() {
    let source = concat!(
        "trait Bound T { check: T -> Bool }\n",
        "trait Other T { verify: T -> Bool }\n",
        "trait Target T { act: T -> T }\n",
        "impl Target I32 {\n",
        "  def act = value => value\n",
        "}\n",
        "impl <T where Bound T> Target T {\n",
        "  def act = value => value\n",
        "}\n",
        "impl <T where Bound T, Other T> Target T {\n",
        "  def act = value => value\n",
        "}\n",
    );
    let root = parse(source).expect("generic trait implementation syntax should parse");
    assert_eq!(root.text(), source);

    let Item::TraitImplementation(concrete) = &root.items[3] else {
        panic!("expected concrete trait implementation");
    };
    assert!(concrete.type_parameters.is_empty());
    assert!(concrete.trait_bounds.is_empty());
    assert_eq!(concrete.trait_name.name, "Target");

    let Item::TraitImplementation(single_bound) = &root.items[4] else {
        panic!("expected generic trait implementation with one bound");
    };
    assert_eq!(single_bound.type_parameters.len(), 1);
    assert_eq!(single_bound.type_parameters[0].names(), ["T"]);
    assert_eq!(single_bound.trait_bounds.len(), 1);
    assert_eq!(single_bound.trait_bounds[0].trait_name.name, "Bound");
    assert_eq!(single_bound.trait_name.name, "Target");

    let Item::TraitImplementation(multi_bound) = &root.items[5] else {
        panic!("expected generic trait implementation with multiple bounds");
    };
    assert_eq!(multi_bound.type_parameters.len(), 1);
    assert_eq!(multi_bound.trait_bounds.len(), 2);
    assert_eq!(multi_bound.trait_bounds[0].trait_name.name, "Bound");
    assert_eq!(multi_bound.trait_bounds[1].trait_name.name, "Other");
    assert_eq!(multi_bound.trait_name.name, "Target");
}

#[test]
fn parses_curried_and_product_trait_parameters_and_arguments() {
    let source = concat!(
        "trait Add Left Right Output { add: (Left, Right) -> Output }\n",
        "trait Convert (From, To) { convert: From -> To }\n",
        "impl Add I32 I32 I32 { def add = (left, right) => left + right }\n",
        "impl Convert (I32, String) { def convert = value => \"converted\" }\n",
        "def combine: <L, R, O where Add L R O> (L, R) -> O = pair => Add.add pair\n",
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

    let Item::Binding(combine) = unmodified_item(&root.items[4]) else {
        panic!("expected bounded function");
    };
    assert_eq!(combine.trait_bounds[0].arguments.len(), 3);
}

#[test]
fn parses_trait_prerequisites_losslessly() {
    let source = concat!(
        "trait Ord T where Eq T { compare: T -> T -> I32 }\n",
        "trait Relation (Left, Right) where Eq Left, Eq Right { related: (Left, Right) -> Bool }\n",
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
        "trait Iterator Iter Item where Iter ~> Item { next: Iter -> Item }\n",
        "trait Add Left Right Output where {Left, Right} ~> Output, Eq Output { add: Left -> Right -> Output }\n",
        "def iterate: <Iter where Iterator Iter> Iter -> () = value => ()\n",
        "def add: <T where Add T T _> T -> T = value => value\n",
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

    let Item::Binding(iterate) = unmodified_item(&root.items[2]) else {
        panic!("expected iterate binding");
    };
    assert_eq!(iterate.trait_bounds[0].arguments.len(), 1);
    let Item::Binding(add_use) = unmodified_item(&root.items[3]) else {
        panic!("expected add binding");
    };
    assert_eq!(add_use.trait_bounds[0].arguments.len(), 3);
}

#[test]
fn parses_default_trait_members_losslessly() {
    let source = concat!(
        "trait Increment T {\n",
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
        "use path.to.another_module.*\n",
        "use path.to.another_module.(func, MyType)\n",
        "use path.to.another_module.func\n",
        "use path.to.another_module.func as my_func\n",
        "pub use path.to.another_module.PublicType\n",
        "pub type alias PublicType = I32\n",
        "pub def public_value = 1\n",
    );
    let root = parse(source).expect("module syntax should parse");

    assert_eq!(root.text(), source);
    assert!(
        matches!(root.items[0], Item::UseDeclaration(ref use_) if use_.kind == UseKind::Dotted)
    );
    assert!(matches!(root.items[1], Item::UseDeclaration(ref use_) if use_.kind == UseKind::Glob));
    assert!(
        matches!(root.items[2], Item::UseDeclaration(ref use_) if matches!(&use_.kind, UseKind::Selected(names) if names == &["func", "MyType"]))
    );
    assert!(
        matches!(root.items[3], Item::UseDeclaration(ref use_) if use_.kind == UseKind::Dotted)
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
    let Item::Binding(binding) = unmodified_item(&root.items[7]) else {
        panic!("expected binding")
    };
    assert_eq!(binding.visibility, Visibility::Public);
    assert!(binding.syntax.text().ends_with("pub def public_value = 1"));
}

#[test]
fn rejects_space_separated_single_item_imports() {
    let error = parse("use path.to.module item\n")
        .expect_err("space-separated item imports should be rejected");
    assert!(error.message.contains("item imports use `.`"));
}

#[test]
fn parses_recursive_inline_submodules_losslessly() {
    let source = concat!(
        "pub mod outer {\n",
        "    use super.parent\n",
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
fn parses_current_module_declarations_losslessly() {
    let private = parse("mod\npub let answer = 42\n").expect("private module should parse");
    assert_eq!(private.visibility, Visibility::Private);
    assert!(private.declaration_syntax.is_some());
    assert_eq!(private.text(), "mod\npub let answer = 42\n");

    let public = parse("@doc(\"API\")\npub mod\npub let answer = 42\n")
        .expect("modified public module should parse");
    assert_eq!(public.visibility, Visibility::Public);
    assert_eq!(public.modifiers.len(), 1);
    assert_eq!(public.modifiers[0].name, "doc");

    assert!(parse("let answer = 42\npub mod\n").is_err());
    assert!(parse("pub mod\nmod\n").is_err());
    assert!(parse("mod {}\n").is_err());

    let inline =
        parse("pub mod api {\n    @doc(\"API module\")\n    mod\n    pub let answer = 42\n}\n")
            .expect("an inline module may declare its current-module metadata");
    let Item::Submodule(api) = &inline.items[0] else {
        panic!("expected inline module")
    };
    assert_eq!(api.module.modifiers.len(), 1);
}

#[test]
fn parses_package_visibility_losslessly() {
    let source = concat!(
        "pub(package) mod internal { pub(package) def value = 1 }\n",
        "pub(package) use internal.value\n",
        "pub(package) type Hidden = I32\n",
        "pub(repr(package)) type Shared = I32\n",
    );
    let root = parse(source).expect("package visibility should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(&root.items[0], Item::Submodule(module) if module.visibility == Visibility::Package));
    assert!(matches!(&root.items[1], Item::UseDeclaration(use_) if use_.visibility == Visibility::Package));
    assert!(matches!(&root.items[2], Item::TypeDeclaration(declaration) if declaration.visibility == Visibility::Package));
    assert!(matches!(&root.items[3], Item::TypeDeclaration(declaration)
        if declaration.visibility == Visibility::Public
            && declaration.representation_visibility == Visibility::Package));

    assert!(parse("pub(repr(package)) def invalid = 1\n").is_err());
    assert!(parse("pub(package(repr)) type Invalid = I32\n").is_err());
}

#[test]
fn parses_block_scoped_submodules_losslessly() {
    let source = "let x = {\n    mod foo { pub def value = 42 }\n    0\n}\n";
    let root = parse(source).expect("block-scoped submodules should parse");
    assert_eq!(root.text(), source);
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
        panic!("expected binding")
    };
    let Some(Expression::Block(block)) = &binding.value else {
        panic!("expected block value")
    };
    let Item::Submodule(submodule) = &block.items[0] else {
        panic!("expected block-scoped submodule");
    };
    assert_eq!(submodule.name, "foo");
    assert_eq!(submodule.visibility, Visibility::Private);
    assert!(matches!(submodule.module.items[0], Item::Binding(_)));
    assert!(matches!(block.items[1], Item::Expression(_)));

    assert!(parse("{ pub mod foo {} }\n").is_err());
}

#[test]
fn parses_block_scoped_type_declarations_losslessly() {
    let source = "let x = {\n    type Wrapped = I32\n    0\n}\n";
    let root = parse(source).expect("block-scoped type declarations should parse");
    assert_eq!(root.text(), source);
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
        panic!("expected binding")
    };
    let Some(Expression::Block(block)) = &binding.value else {
        panic!("expected block value")
    };
    let Item::TypeDeclaration(declaration) = &block.items[0] else {
        panic!("expected block-scoped type declaration");
    };
    assert_eq!(declaration.name, "Wrapped");
    assert_eq!(declaration.kind, TypeDeclarationKind::Distinct);
    assert_eq!(declaration.visibility, Visibility::Private);
    assert!(matches!(block.items[1], Item::Expression(_)));

    assert!(parse("{ pub type Foo = I32 }\n").is_err());
}

#[test]
fn parses_block_scoped_use_declarations_losslessly() {
    let source = concat!(
        "let x = {\n",
        "    use path.to.another_module\n",
        "    use path.to.another_module.*\n",
        "    use path.to.another_module.(func, MyType)\n",
        "    use path.to.another_module.func as my_func\n",
        "    0\n",
        "}\n",
    );
    let root = parse(source).expect("block-scoped use declarations should parse");
    assert_eq!(root.text(), source);
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
        panic!("expected binding")
    };
    let Some(Expression::Block(block)) = &binding.value else {
        panic!("expected block value")
    };
    assert!(matches!(
        &block.items[0],
        Item::UseDeclaration(use_) if use_.kind == UseKind::Dotted
    ));
    assert!(matches!(
        &block.items[1],
        Item::UseDeclaration(use_) if use_.kind == UseKind::Glob
    ));
    assert!(matches!(
        &block.items[2],
        Item::UseDeclaration(use_)
            if matches!(&use_.kind, UseKind::Selected(names) if names == &["func", "MyType"])
    ));
    assert!(matches!(
        &block.items[3],
        Item::UseDeclaration(use_)
            if matches!(&use_.kind, UseKind::Renamed { item, alias } if item == "func" && alias == "my_func")
    ));
    let Item::UseDeclaration(use_) = &block.items[0] else {
        panic!("expected use declaration")
    };
    assert_eq!(use_.visibility, Visibility::Private);
    assert!(matches!(block.items[4], Item::Expression(_)));

    assert!(parse("{ pub use path.to.another_module }\n").is_err());
}

#[test]
fn parses_namespace_qualified_types() {
    let root = parse("let value: types.Number = 1\n").expect("qualified type should parse");
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
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
        parse("pub type CPointer Pointee = opaque\n").expect("generic opaque type should parse");
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
fn parses_the_opaque_parse_quote_signature() {
    let source = "pub macro parse_quote: Braced Syntax -> Syntax\n";
    let root = parse(source).expect("primitive macro signature should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        root.items[0],
        Item::MacroDeclaration(ref declaration)
            if declaration.name == "parse_quote"
                && declaration.type_parameters.is_empty()
                && declaration.trait_bounds.is_empty()
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
            if matches!(item.as_ref(), item
                if matches!(item, Item::Binding(_)))
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
        "macro @replace: Parenthesized (Expr) -> Item -> Item = value => item => quote { let generated = $value }\n",
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
    assert!(matches!(modified.item.as_ref(), Item::Binding(_)));
}

#[test]
fn parses_triple_slash_docs_on_named_declarations_and_members() {
    let source = concat!(
        "/// Type line 1\r\n",
        "/// Type line 2\r\n",
        "pub type Documented = I32\r\n",
        "trait Example T {\r\n",
        "  /// Member docs\r\n",
        "  member: T -> T\r\n",
        "}\r\n",
        "//// ordinary comment\r\n",
        "def plain = 0\r\n",
    );
    let module = parse(source).expect("doc comments should parse");
    assert_eq!(module.text(), source);
    let Item::TypeDeclaration(documented) = &module.items[0] else {
        panic!("expected documented type declaration");
    };
    assert_eq!(documented.docs, [" Type line 1", " Type line 2"]);
    let Item::TraitDeclaration(declaration) = &module.items[1] else {
        panic!("expected trait declaration");
    };
    assert_eq!(declaration.members[0].docs, [" Member docs"]);
    let Item::Binding(binding) = &module.items[2] else {
        panic!("expected binding");
    };
    assert!(binding.docs.is_empty());
}

#[test]
fn parses_metadata_aware_macro_calls_losslessly() {
    let source = concat!(
        "macro define = metadata: MacroCallMetadata => ty: Type => quote { type Generated = $ty }\n",
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
            if matches!(item.as_ref(), Item::TypeDeclaration(_))
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
fn parses_modifier_prefixes_as_macro_call_metadata() {
    let source = concat!(
        "macro define = MacroCallMetadata => quote { let generated: I32 = 42 }\n",
        "/// generated documentation\n",
        "@outer\n",
        "@inner\n",
        "pub define\n",
    );
    let root = parse(source).expect("metadata prefixes should parse");
    assert_eq!(root.text(), source);
    let Item::VisibilityMacroInvocation(invocation) = &root.items[1] else {
        panic!("expected metadata macro invocation");
    };
    assert_eq!(invocation.visibility.kind, stapler::VisibilityKind::Public);
    assert_eq!(invocation.modifiers.len(), 3);
    assert_eq!(invocation.modifiers[0].name, "doc");
    assert_eq!(invocation.modifiers[1].name, "outer");
    assert_eq!(invocation.modifiers[2].name, "inner");
}

#[test]
fn parses_doc_comments_before_metadata_calls() {
    let source = concat!(
        "macro define = MacroCallMetadata => quote { let generated: I32 = 42 }\n",
        "/// generated documentation\n",
        "pub define\n",
    );
    let root = parse(source).expect("documentation should become metadata");
    let Item::VisibilityMacroInvocation(invocation) = &root.items[1] else {
        panic!("expected metadata macro invocation");
    };
    assert_eq!(invocation.modifiers.len(), 1);
    assert_eq!(invocation.modifiers[0].name, "doc");
    assert_eq!(invocation.modifiers[0].doc.as_deref(), Some(" generated documentation"));
}

#[test]
fn preserves_doc_comments_interleaved_with_metadata_modifiers() {
    let source = concat!(
        "macro define = MacroCallMetadata => quote { let generated: I32 = 42 }\n",
        "@outer\n",
        "/// generated documentation\n",
        "@inner\n",
        "pub define\n",
    );
    let root = parse(source).expect("interleaved metadata prefixes should parse");
    let Item::VisibilityMacroInvocation(invocation) = &root.items[1] else {
        panic!("expected metadata macro invocation");
    };
    assert_eq!(
        invocation
            .modifiers
            .iter()
            .map(|modifier| modifier.name.as_str())
            .collect::<Vec<_>>(),
        ["outer", "doc", "inner"]
    );
}

#[test]
fn parses_declaration_style_item_macro_punctuation_losslessly() {
    let source = concat!(
        "typegroup Local { Unit, }\n",
        "pub(repr) typegroup Result (T, E,) { Ok T, Err E, }\n",
        "value = 1\n",
        "identity = argument => argument\n",
    );
    let root = parse(source).expect("declaration-style macro calls should parse");
    assert_eq!(root.text(), source);
    assert!(matches!(
        &root.items[0],
        item
            if matches!(item, Item::Expression(Expression::Call(_)))
    ));
    assert!(matches!(
        &root.items[1],
        Item::VisibilityMacroInvocation(invocation)
            if invocation.visibility.kind == stapler::VisibilityKind::PublicRepr
    ));
    assert!(matches!(
        unmodified_item(&root.items[2]),
        Item::Assignment(_)
    ));
    assert!(matches!(
        unmodified_item(&root.items[3]),
        Item::Assignment(_)
    ));
}

#[test]
fn parses_type_companion_blocks_losslessly() {
    let source = concat!(
        "companion Animal { pub def move_to = animal => animal }\n",
        "companion<T where Bound T> Box T { def value = () => 1 }\n",
    );
    let root = parse(source).expect("companion blocks should parse");
    assert_eq!(root.text(), source);
    assert!(
        matches!(&root.items[0], Item::Submodule(module) if module.companion && module.name == "Animal")
    );
    assert!(
        matches!(&root.items[1], Item::Submodule(module) if module.companion && module.name == "Box")
    );
    assert!(parse("pub companion Animal {}\n").is_err());
    assert!(parse("companion (I32, I32) {}\n").is_err());
}

#[test]
fn parses_companion_method_calls_at_postfix_precedence() {
    let source = "animal^move_to (1.0, 1.0)\n";
    let root = parse(source).expect("companion method call should parse");
    assert_eq!(root.text(), source);
    let Item::Expression(Expression::Call(call)) = &root.items[0] else {
        panic!("expected explicit argument call");
    };
    let Expression::Call(method) = call.callee.as_ref() else {
        panic!("expected receiver application");
    };
    assert!(matches!(
        method.callee.as_ref(),
        Expression::Access(access) if access.accessor == Accessor::Method("move_to".into())
    ));
    assert!(parse("animal^ 1\n").is_err());
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
    let item = &root.items[0];
    let Item::Expression(Expression::Call(call)) = item else {
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
        ref item
            if matches!(item, Item::Expression(Expression::Call(_)))
    ));
}

#[test]
fn parses_product_parameter_and_expression_body() {
    let source = "def add: _ -> I32 = (a: I32, b: I32) => a + b\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
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
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
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
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
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
    let Item::Expression(Expression::Call(outer)) = unmodified_item(&root.items[0]) else {
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
fn parses_logical_and_or_as_dedicated_nodes_not_calls() {
    let source = "a && b || c\n";
    let root = parse(source).expect("logical operator expression should parse");
    let Item::Expression(Expression::Logical(or)) = unmodified_item(&root.items[0]) else {
        panic!("expected top-level `||` as a `Logical` node, not a desugared call");
    };
    assert_eq!(or.operator, LogicalOperator::Or);
    let Expression::Logical(and) = or.left.as_ref() else {
        panic!("expected `&&` to bind tighter than `||`");
    };
    assert_eq!(and.operator, LogicalOperator::And);
    assert!(matches!(and.left.as_ref(), Expression::Name(name) if name.name == "a"));
    assert!(matches!(and.right.as_ref(), Expression::Name(name) if name.name == "b"));
    assert!(matches!(or.right.as_ref(), Expression::Name(name) if name.name == "c"));
    assert_eq!(root.text(), source);
}

#[test]
fn parses_left_associative_logical_and_chains() {
    let source = "a && b && c\n";
    let root = parse(source).expect("chained `&&` should parse");
    let Item::Expression(Expression::Logical(outer)) = unmodified_item(&root.items[0]) else {
        panic!("expected `&&` chain to parse as `Logical` nodes");
    };
    assert_eq!(outer.operator, LogicalOperator::And);
    assert!(matches!(outer.right.as_ref(), Expression::Name(name) if name.name == "c"));
    let Expression::Logical(inner) = outer.left.as_ref() else {
        panic!("expected `a && b` nested under the outer `&&`, confirming left-associativity");
    };
    assert_eq!(inner.operator, LogicalOperator::And);
    assert!(matches!(inner.left.as_ref(), Expression::Name(name) if name.name == "a"));
    assert!(matches!(inner.right.as_ref(), Expression::Name(name) if name.name == "b"));
}

#[test]
fn logical_operators_bind_looser_than_comparisons() {
    let source = "1 == 1 && 2 == 2\n";
    let root = parse(source).expect("`&&` mixed with comparisons should parse");
    let Item::Expression(Expression::Logical(and)) = unmodified_item(&root.items[0]) else {
        panic!("expected `&&` at the top, binding looser than `==`");
    };
    assert_eq!(and.operator, LogicalOperator::And);
    assert!(matches!(and.left.as_ref(), Expression::Call(_)));
    assert!(matches!(and.right.as_ref(), Expression::Call(_)));
}

#[test]
fn allows_chaining_logical_operators_unlike_comparisons() {
    parse("a && b && c || d\n").expect("`&&`/`||` are associative and may be chained freely");
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
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
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
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
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
        "def copy = outer@(left, right) => outer\n",
    );
    let root = parse(source).expect("at-patterns should parse");
    assert_eq!(root.text(), source);

    let Item::PatternBinding(point) = unmodified_item(&root.items[0]) else {
        panic!("expected destructuring binding");
    };
    assert!(matches!(point.pattern, Pattern::At(_)));

    let Item::PatternBinding(outer_let) = unmodified_item(&root.items[1]) else {
        panic!("expected destructuring binding");
    };
    let Pattern::At(outer) = &outer_let.pattern else {
        panic!("expected outer at-pattern");
    };
    assert!(outer.binding.mutable);
    assert!(matches!(outer.pattern.as_ref(), Pattern::At(_)));

    let Item::Binding(copy) = unmodified_item(&root.items[2]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &copy.value else {
        panic!("expected function");
    };
    let Pattern::At(parameter) = &function.pattern else {
        panic!("expected parameter at-pattern");
    };
    assert_eq!(parameter.binding.name, "outer");

    assert!(parse("let _@(x, y) = (1, 2)\n").is_err());
    assert!(parse("let (x, y)@point = (1, 2)\n").is_err());
    assert!(parse("let point@ = (1, 2)\n").is_err());
    let marked = parse("def marked = mut outer@(left, right) => outer\n")
        .expect("a top-level at-pattern alias may declare whole-parameter mutation");
    let Item::Binding(marked) = unmodified_item(&marked.items[0]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &marked.value else {
        panic!("expected function");
    };
    assert!(matches!(&function.pattern, Pattern::At(at) if at.binding.mutable));
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
        "def args: (name: String, int)\n",
        "type user_id = int\n",
        "def value = (name: \"staple\", 1)\n",
        "def by_name = args.name\n",
        "def by_index = args.1\n",
    );
    let root = parse(source).expect("named product syntax should parse");

    assert_eq!(root.text(), source);
    let Item::Binding(args) = unmodified_item(&root.items[0]) else {
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

    let Item::Binding(value) = unmodified_item(&root.items[2]) else {
        panic!("expected value binding");
    };
    let Expression::Product(value) = value.value.as_ref().expect("product value") else {
        panic!("expected product value");
    };
    assert_eq!(value.elements[0].name.as_deref(), Some("name"));

    let Item::Binding(by_name) = unmodified_item(&root.items[3]) else {
        panic!("expected name access binding");
    };
    assert!(matches!(
        by_name.value,
        Some(Expression::Access(ref access)) if access.accessor == Accessor::Name("name".into())
    ));

    let Item::Binding(by_index) = unmodified_item(&root.items[4]) else {
        panic!("expected index access binding");
    };
    assert!(matches!(
        by_index.value,
        Some(Expression::Access(ref access)) if access.accessor == Accessor::Index("1".into())
    ));
}

#[test]
fn parses_contextual_named_product_initializers_losslessly() {
    let source = "let value: (I32, a: I32, b: I32) = (1, .b: 3, .a: 2)\n";
    let root = parse(source).expect("designated product syntax should parse");
    assert_eq!(root.text(), source);
    let Item::Binding(value) = unmodified_item(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Product(product)) = &value.value else {
        panic!("expected product expression");
    };
    assert!(!product.elements[0].designated);
    assert!(product.elements[1].designated);
    assert_eq!(product.elements[1].name.as_deref(), Some("b"));
    assert!(product.elements[2].designated);
    assert_eq!(product.elements[2].name.as_deref(), Some("a"));
}

#[test]
fn rejects_positional_elements_after_a_designated_initializer() {
    let error = parse("let value: (a: I32, I32) = (.a: 1, 2)\n")
        .expect_err("positional suffix should be rejected");
    assert!(
        error
            .message
            .contains("must precede designated initializers")
    );
}

#[test]
fn type_declaration_underlying_type_stops_at_newline() {
    let source = concat!(
        "type Test = ()\n",
        "\n",
        "std.io.println \"Hello, world!\"\n",
    );
    let root = parse(source).expect("item after a type alias should parse");
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

    let item = &root.items[1];
    let Item::Expression(Expression::Call(call)) = item else {
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
        "type alias Pair (A, B) = (A, B)\n",
        "type Box T = (value: T)\n",
        "def identity: <T> T -> T = x => x\n",
        "def pair: Pair (String, I32)\n",
    );
    let root = parse(source).expect("generic syntax should parse");
    assert_eq!(root.text(), source);
    let Item::TypeDeclaration(pair) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert_eq!(pair.type_parameters.len(), 1);
    let Item::Binding(identity) = unmodified_item(&root.items[2]) else {
        panic!("expected generic function binding");
    };
    assert_eq!(identity.type_parameters.len(), 1);
    let Item::Binding(value) = unmodified_item(&root.items[3]) else {
        panic!("expected annotated binding");
    };
    assert!(matches!(value.annotation, Some(Type::Application(_))));
}

#[test]
fn parses_default_type_bounds_losslessly() {
    let source = concat!(
        "type Box (T = String) = (value: T)\n",
        "type alias Pair A (B = A) = (A, B)\n",
        "trait Increment (T = I32) { increment: T -> T }\n",
    );
    let root = parse(source).expect("default type bounds should parse");
    assert_eq!(root.text(), source);

    let Item::TypeDeclaration(boxed) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert_eq!(boxed.default_bounds.len(), 1);
    assert_eq!(boxed.default_bounds[0].parameter.name, "T");
    assert!(matches!(boxed.default_bounds[0].default, Type::Named(_)));

    let Item::TypeDeclaration(pair) = &root.items[1] else {
        panic!("expected type alias declaration");
    };
    assert_eq!(pair.default_bounds.len(), 1);
    assert_eq!(pair.default_bounds[0].parameter.name, "B");

    let Item::TraitDeclaration(increment) = &root.items[2] else {
        panic!("expected trait declaration");
    };
    assert_eq!(increment.default_bounds.len(), 1);
    assert_eq!(increment.default_bounds[0].parameter.name, "T");
}

#[test]
fn parses_inline_default_type_bounds_losslessly() {
    let source = concat!(
        "pub(repr) type Ident (Spelling = String) where Spelling <: String = Spelling\n",
        "type alias Pair A (B = A) = (A, B)\n",
        "trait Converts From (To = String) { convert: From -> To }\n",
    );
    let root = parse(source).expect("inline default type bounds should parse");
    assert_eq!(root.text(), source);

    let Item::TypeDeclaration(ident) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert_eq!(ident.type_parameters.len(), 1);
    assert_eq!(ident.default_bounds.len(), 1);
    assert_eq!(ident.default_bounds[0].parameter.name, "Spelling");
    assert_eq!(ident.subtype_bounds.len(), 1);
    assert_eq!(ident.subtype_bounds[0].parameter.name, "Spelling");

    let Item::TypeDeclaration(pair) = &root.items[1] else {
        panic!("expected type alias declaration");
    };
    assert_eq!(pair.type_parameters.len(), 2);
    assert_eq!(pair.default_bounds.len(), 1);
    assert_eq!(pair.default_bounds[0].parameter.name, "B");

    let Item::TraitDeclaration(converts) = &root.items[2] else {
        panic!("expected trait declaration");
    };
    assert_eq!(converts.type_parameters.len(), 2);
    assert_eq!(converts.default_bounds.len(), 1);
    assert_eq!(converts.default_bounds[0].parameter.name, "To");
}

#[test]
fn rejects_inline_default_on_a_product_type_parameter() {
    assert!(parse("type alias Bad = (A, B) ?= (I32, I32) => (A, B)\n").is_err());
}

#[test]
fn combines_inline_and_trailing_default_type_bounds() {
    let source = concat!(
        "type alias Triple (A = I32) B (C = B) = (A, B, C)\n",
        "type alias Triple (A = I32) (B = A) = (A, B)\n",
    );
    let root = parse(source).expect("mixed inline and trailing defaults should parse");
    assert_eq!(root.text(), source);

    let Item::TypeDeclaration(triple) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert_eq!(triple.type_parameters.len(), 3);
    assert_eq!(triple.default_bounds.len(), 2);
    assert_eq!(triple.default_bounds[0].parameter.name, "A");
    assert_eq!(triple.default_bounds[1].parameter.name, "C");
}

#[test]
fn parses_sized_relaxations_losslessly() {
    let source = concat!(
        "pub(repr) type RefLike T where ?Sized T = T\n",
        "def preserve: <T where ?Sized T> Ref T -> Ref T = value => value\n",
        "def ordinary: <T> T -> T = value => value\n",
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

    let Item::Binding(preserve) = unmodified_item(&root.items[1]) else {
        panic!("expected generic binding");
    };
    assert!(matches!(
        preserve.type_parameters.as_slice(),
        [stapler::TypeParameterPattern::Binding(binding)] if !binding.sized
    ));

    let Item::Binding(ordinary) = unmodified_item(&root.items[2]) else {
        panic!("expected generic binding");
    };
    assert!(matches!(
        ordinary.type_parameters.as_slice(),
        [stapler::TypeParameterPattern::Binding(binding)] if binding.sized
    ));

    assert!(parse("type alias Bad T where ?Other T = T\n").is_err());
    assert!(parse("type alias Bad T where ?Sized U = T\n").is_err());
    assert!(parse("type alias Bad T where ?Sized T, ?Sized T = T\n").is_err());
}

#[test]
fn parses_public_representations_and_nominal_patterns() {
    let source = concat!(
        "pub(repr) type Box T = (value: T)\n",
        "let Box (value) = Box (value: 42)\n",
        "def unbox: Box I32 -> I32 = Box value => value\n",
    );
    let root = parse(source).expect("nominal patterns should parse");
    let Item::TypeDeclaration(declaration) = &root.items[0] else {
        panic!("expected type declaration");
    };
    assert_eq!(declaration.representation_visibility, Visibility::Public);
    assert!(matches!(
        unmodified_item(&root.items[1]),
        Item::PatternBinding(binding)
            if matches!(binding.pattern, Pattern::Nominal(_))
    ));
    let Item::Binding(binding) = unmodified_item(&root.items[2]) else {
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
    let Item::Binding(binding) = unmodified_item(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected block");
    };
    assert!(matches!(block.items[0], Item::Binding(_)));
}

#[test]
fn rejects_unsupported_items_in_blocks() {
    for source in [
        "{ extern \"c\" {} }\n",
        "{ macro generated = () }\n",
        "{ trait Generated {} }\n",
        "{ impl Generated I32 {} }\n",
    ] {
        assert!(
            parse(source).is_err(),
            "unsupported block item parsed: {source}"
        );
    }
}

#[test]
fn parses_modifiers_on_block_items() {
    let source = "{ @outer @inner(42) let value = 1 }\n";
    let root = parse(source).expect("block item modifiers should parse");
    let Item::Expression(Expression::Block(block)) = &root.items[0] else {
        panic!("expected block expression");
    };
    let Item::Modified(modified) = &block.items[0] else {
        panic!("expected modified block item");
    };
    assert_eq!(modified.modifiers.len(), 2);
    assert!(matches!(modified.item.as_ref(), Item::Binding(_)));
}

#[test]
fn parses_multiple_top_level_items() {
    let source = "let greeting = \"hello\"\nprintln greeting\nprintln \"second\"\n";
    let root = parse(source).expect("top-level items should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 3);
    assert!(matches!(
        root.items[0],
        ref item if matches!(item, Item::Binding(_))
    ));
    assert!(matches!(root.items[1], Item::Expression(_)));
    assert!(matches!(root.items[2], Item::Expression(_)));
}

#[test]
fn parses_returns_and_semicolon_separated_items_losslessly() {
    let source = concat!(
        "use std.core.*;",
        "type alias Number = I32;",
        "extern \"c\" { exit: I32 -> (); };",
        "def answer = () => { let value = 42; return value; };",
        "answer ();",
    );
    let root = parse(source).expect("semicolon-separated source should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 5);
    let Item::Binding(answer) = unmodified_item(&root.items[3]) else {
        panic!("expected answer binding");
    };
    let Some(Expression::Function(function)) = &answer.value else {
        panic!("expected function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected function block");
    };
    assert!(matches!(block.items[1], Item::Return(_)));
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
    let Item::Binding(read) = unmodified_item(&root.items[0]) else {
        panic!("expected read binding");
    };
    assert!(
        matches!(read.annotation, Some(Type::Function(ref function)) if matches!(function.result.as_ref(), Type::Sum(_)))
    );
    let Item::Binding(parse_binding) = unmodified_item(&root.items[1]) else {
        panic!("expected parse binding");
    };
    let Some(Expression::Function(function)) = &parse_binding.value else {
        panic!("expected function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected block");
    };
    assert!(matches!(
        block.items[0],
        Item::PatternBinding(ref binding)
            if binding.kind == stapler::PatternBindingKind::Propagating
    ));
}

#[test]
fn parses_repeated_spread_erased_and_variable_index_syntax() {
    let source = concat!(
        "def fixed: Ref I32[3]\n",
        "def erased: Ref I32[]\n",
        "def mixed: (String, ...I32[3], ...(I32, I32))\n",
        "let expanded = (prefix: \"value\", ...mixed, suffix: 1)\n",
        "let value = erased[index].0\n",
    );
    let module = parse(source).expect("product extensions should parse");
    assert_eq!(module.text(), source);

    let Item::Binding(fixed) = unmodified_item(&module.items[0]) else {
        panic!("expected fixed binding");
    };
    assert!(matches!(fixed.annotation, Some(Type::Application(_))));

    let Item::Binding(expanded) = unmodified_item(&module.items[3]) else {
        panic!("expected expanded binding");
    };
    let Some(Expression::Product(expanded)) = &expanded.value else {
        panic!("expected expanded product value");
    };
    assert!(expanded.elements[1].spread);

    let Item::Binding(value) = unmodified_item(&module.items[4]) else {
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

    let Item::Binding(config) = unmodified_item(&module.items[1]) else {
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
fn parses_mutable_patterns_and_assignment_items_losslessly() {
    let source = concat!(
        "let mut value = 1\n",
        "let (mut left, right) = (2, 3)\n",
        "def update = (parameter: I32) => { let mut parameter = parameter; parameter = 4; parameter }\n",
        "value = left\n",
    );
    let module = parse(source).expect("mutable bindings and assignments should parse");
    assert_eq!(module.text(), source);
    let Item::Binding(value) = unmodified_item(&module.items[0]) else {
        panic!("expected mutable binding");
    };
    assert!(value.mutable);
    let Item::PatternBinding(pair) = unmodified_item(&module.items[1]) else {
        panic!("expected pattern binding");
    };
    let Pattern::Product(pattern) = &pair.pattern else {
        panic!("expected product pattern");
    };
    assert!(matches!(&pattern.elements[0], Pattern::Binding(binding) if binding.mutable));
    assert!(matches!(
        unmodified_item(&module.items[3]),
        Item::Assignment(_)
    ));
    assert!(parse("extern \"c\" { mut value: I32 }\n").is_err());
    let parameter = parse("def update = (mut parameter: I32) => { parameter = 4 }\n")
        .expect("a direct parameter binding may declare mutation permission");
    let Item::Binding(update) = unmodified_item(&parameter.items[0]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &update.value else {
        panic!("expected function expression");
    };
    let Pattern::Product(parameters) = &function.pattern else {
        panic!("expected product parameter");
    };
    assert!(matches!(&parameters.elements[0], Pattern::Binding(binding) if binding.mutable));
    assert!(parse("def bad = ((mut nested: I32, other: I32), last: I32) => nested\n").is_err());
}

#[test]
fn parses_per_name_mut_pattern_forms_losslessly() {
    let source = concat!(
        "let (mut alpha, mut beta) = (5, 6)\n",
        "def update = (parameter: I32) => { let mut parameter = parameter; parameter = 7; parameter }\n",
        "match update 0 { Box (mut inner) => inner, _ => 0 }\n",
    );
    let module = parse(source).expect("mut pattern forms should parse");
    assert_eq!(module.text(), source);

    let Item::PatternBinding(pair) = unmodified_item(&module.items[0]) else {
        panic!("expected per-name pattern binding");
    };
    let Pattern::Product(pattern) = &pair.pattern else {
        panic!("expected product pattern");
    };
    assert!(pattern.elements.iter().all(|element| matches!(
        element,
        Pattern::Binding(binding) if binding.mutable
    )));

    let Item::Binding(update) = unmodified_item(&module.items[1]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &update.value else {
        panic!("expected function value");
    };
    let Pattern::Product(parameters) = &function.pattern else {
        panic!("expected product pattern");
    };
    assert!(matches!(&parameters.elements[0], Pattern::Binding(binding) if !binding.mutable));

    // `var` is no longer a keyword; the whole-pattern propagation it used to
    // provide over a destructuring `let` has no `mut` equivalent.
    assert!(parse("let mut (a, b) = pair\n").is_err());
    assert!(parse("let (mut Box inner, extra) = pair\n").is_err());
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
    let Item::Binding(binding) = unmodified_item(&module.items[0]) else {
        panic!("expected function binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function expression");
    };
    let Expression::Loop(loop_) = function.body.as_ref() else {
        panic!("expected loop expression");
    };
    assert!(matches!(
        loop_.body.items[0],
        Item::Expression(Expression::Loop(_))
    ));
    assert!(matches!(
        loop_.body.items[1],
        Item::Break(ref break_) if break_.value.is_some()
    ));
    assert!(matches!(loop_.body.items[2], Item::Continue(_)));
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
    let Item::Binding(binding) = unmodified_item(&module.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };
    let Expression::Loop(loop_) = function.body.as_ref() else {
        panic!("expected loop");
    };
    assert!(matches!(
        loop_.body.items[0],
        Item::Break(ref break_) if break_.value.is_none()
    ));
    assert!(matches!(loop_.body.items[1], Item::Expression(_)));
    assert!(parse("def invalid = () => loop { continue 42 }\n").is_err());
}

#[test]
fn parses_const_bindings_losslessly_at_top_level_and_in_blocks() {
    let source = concat!(
        "const x: I32 = 1 + 3\n",
        "def wrapper = () => {\n",
        "  const y = x + 1\n",
        "  y\n",
        "}\n",
    );
    let module = parse(source).expect("const bindings should parse");
    assert_eq!(module.syntax.text(), source);

    let Item::Binding(x) = unmodified_item(&module.items[0]) else {
        panic!("expected const binding");
    };
    assert_eq!(x.kind, BindingKind::Const);
    assert_eq!(x.name, "x");
    assert!(x.value.is_some());

    let Item::Binding(wrapper) = unmodified_item(&module.items[1]) else {
        panic!("expected wrapper binding");
    };
    let Some(Expression::Function(function)) = &wrapper.value else {
        panic!("expected wrapper function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected block body");
    };
    let Item::Binding(y) = &block.items[0] else {
        panic!("expected local const binding");
    };
    assert_eq!(y.kind, BindingKind::Const);
}

#[test]
fn rejects_malformed_const_bindings() {
    assert!(parse("const x: I32\n").is_err());
    assert!(parse("const mut x = 1\n").is_err());
    assert!(parse("const x<T> = 1\n").is_err());
}

#[test]
fn reserves_package_while_allowing_package_qualified_paths() {
    let source = "use package.utils.add\nlet value: package.models.Value = package.utils.add 1\n";
    let module = parse(source).expect("package-qualified paths should parse");
    assert_eq!(module.syntax.text(), source);
    assert!(
        module
            .syntax
            .tokens()
            .iter()
            .any(|token| token.kind == TokenKind::Package)
    );
    assert!(parse("let package = 1\n").is_err());
}
