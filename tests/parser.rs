use stapler::{
    Accessor, BinaryOperator, Expression, Item, Pattern, Statement, TokenKind, Type,
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
        "pub type alias PublicType = i32\n",
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
            .ends_with("pub type alias PublicType = i32")
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
fn parses_hello_world_losslessly() {
    let source = include_str!("../examples/hello_world.sta");
    let root = parse(source).expect("hello_world should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 2);
    assert!(matches!(root.items[0], Item::ExternBlock(_)));
    assert!(matches!(
        root.items[1],
        Item::Statement(ref statement)
            if matches!(statement.as_ref(), Statement::Expression(Expression::Call(_)))
    ));
}

#[test]
fn parses_product_parameter_and_expression_body() {
    let source = "def add: _ -> i32 = (a: i32, b: i32) => a + b\n";
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
        Expression::Binary(ref binary) if binary.operator == BinaryOperator::Add
    ));
}

#[test]
fn parses_function_result_annotation_before_body_arrow() {
    let source = "let add = (a: i32, b: i32) -> i32 => a + b\n";
    let root = parse(source).expect("function should parse");
    let Statement::Binding(binding) = statement(&root.items[0]) else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };

    assert!(matches!(
        function.return_type,
        Some(Type::Primitive(stapler::PrimitiveType::I32(_)))
    ));
    assert_eq!(root.text(), source);
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
    let source = "def println: _ -> i32 = (s: string) => printf (\"%s\\n\", s)\n";
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
    let source = "let first = (x: i32, (y: i32, z: i32)) => x + y\n";
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
    let source = "// entry\r\ndef main: _ -> string = () => {\r\n  \"ok\"\r\n}\r\n";
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
        "let args: (name: string, int)\n",
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
