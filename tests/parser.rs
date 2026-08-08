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
fn parses_use_declarations_and_public_items_losslessly() {
    let source = concat!(
        "use path.to.another_module\n",
        "use path.to.another_module.*\n",
        "use path.to.another_module.(func, MyType)\n",
        "use path.to.another_module.func as my_func\n",
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
        matches!(root.items[3], Item::UseDeclaration(ref use_) if matches!(&use_.kind, UseKind::Renamed { item, alias } if item == "func" && alias == "my_func"))
    );
    assert!(
        matches!(root.items[4], Item::TypeDeclaration(ref declaration) if declaration.visibility == Visibility::Public)
    );
    let Item::TypeDeclaration(declaration) = &root.items[4] else {
        panic!("expected type declaration")
    };
    assert!(
        declaration
            .syntax
            .text()
            .ends_with("pub type alias PublicType = I32")
    );
    let Statement::Binding(binding) = statement(&root.items[5]) else {
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
    let root = parse("pub type I32\n").expect("opaque type should parse");
    assert!(matches!(
        root.items[0],
        Item::TypeDeclaration(ref declaration)
            if declaration.kind == TypeDeclarationKind::Opaque
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
    let root = parse("pub macro c_string\n").expect("macro declaration should parse");
    assert!(matches!(
        root.items[0],
        Item::MacroDeclaration(ref declaration)
            if declaration.visibility == Visibility::Public && declaration.name == "c_string"
    ));
}

#[test]
fn parses_hello_world_losslessly() {
    let source = include_str!("../examples/hello_world.sta");
    let root = parse(source).expect("hello_world should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 10);
    assert!(matches!(root.items[0], Item::UseDeclaration(_)));
    assert!(matches!(root.items[1], Item::ExternBlock(_)));
    assert!(matches!(
        root.items[4],
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
fn parses_function_result_annotation_before_body_arrow() {
    let source = "let add = (a: I32, b: I32) -> I32 => a + b\n";
    let root = parse(source).expect("function should parse");
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };

    assert!(matches!(
        function.return_type,
        Some(Type::Named(ref named)) if named.name == "I32"
    ));
    assert_eq!(root.text(), source);
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
fn rejects_block_local_fixity_modifiers() {
    let error = parse("def outer = () => { def infixl 6 + = x: I32 => x }\n")
        .expect_err("block fixity should be rejected");
    assert!(error.message.contains("module level"));
}

#[test]
fn rejects_the_old_function_body_arrow() {
    let error = parse("let answer = () -> 42\n").expect_err("old syntax should not parse");

    assert!(error.message.contains("expected type"));
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
