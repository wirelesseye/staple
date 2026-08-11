use stapler::{
    Accessor, Associativity, Expression, Item, Pattern, Statement, TokenKind, Type,
    TypeDeclarationKind, UseKind, Visibility, parse,
};

fn statement(item: &Item) -> &Statement {
    let Item::Statement(statement) = item else {
        panic!("expected statement");
    };
    statement
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
        "Ident \"else\" => else_branch: Expr => quote { $else_branch }\n",
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
        Pattern::Nominal(ref pattern)
            if pattern.name == "Ident"
                && matches!(pattern.argument.as_ref(), Pattern::StringLiteral(_))
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
    assert!(matches!(quote.template.as_ref(), Expression::Splice(splice) if splice.repeated));
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
    assert!(matches!(
        *function.body,
        Expression::Infix(ref infix)
            if infix.operators.len() == 1 && infix.operators[0].name == "+"
    ));
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
    assert!(matches!(satisfies.value.as_ref(), Expression::Infix(_)));
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
fn parses_inline_fixity_and_operator_call_forms() {
    let source = concat!(
        "def infixl 6 +: I32 -> I32 -> I32 = x => y => x\n",
        "def infixr 5 **: I32 -> I32 -> I32 = x => y => y\n",
        "1 + 2 ** 3\n",
        "1 `combine` 2\n",
        "(+) 1 2\n",
        "(+)\n",
    );
    let root = parse(source).expect("operator syntax should parse");
    let Statement::Binding(plus) = statement(&root.items[0]) else {
        panic!("expected operator binding");
    };
    assert_eq!(plus.name, "+");
    assert_eq!(
        plus.fixity.expect("fixity").associativity,
        Associativity::Left
    );
    assert_eq!(plus.fixity.expect("fixity").precedence, 6);

    let Statement::Expression(Expression::Infix(infix)) = statement(&root.items[2]) else {
        panic!("expected infix chain");
    };
    assert_eq!(
        infix
            .operators
            .iter()
            .map(|op| op.name.as_str())
            .collect::<Vec<_>>(),
        ["+", "**"]
    );
    assert!(matches!(
        statement(&root.items[4]),
        Statement::Expression(Expression::Call(_))
    ));
    assert!(
        matches!(statement(&root.items[5]), Statement::Expression(Expression::Name(name)) if name.name == "+")
    );
    assert_eq!(root.text(), source);
}

#[test]
fn rejects_dot_as_a_binding_name() {
    let error = parse("def .: I32 -> I32 = x => x\n")
        .expect_err("dot should remain punctuation rather than a binding name");

    assert_eq!(error.message, "expected binding name");
}

#[test]
fn rejects_block_local_fixity_modifiers() {
    let error = parse("def outer = () => { def infixl 6 + = x: I32 => x }\n")
        .expect_err("block fixity should be rejected");
    assert!(error.message.contains("module level"));
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
    assert!(parse("pub(repr) type Singleton\n").is_err());
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
fn parses_mutable_patterns_and_assignment_statements_losslessly() {
    let source = concat!(
        "let mut value = 1\n",
        "let (mut left, right) = (2, 3)\n",
        "def update = (mut parameter: I32) => { parameter = 4; parameter }\n",
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
