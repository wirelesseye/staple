use stapler::{
    Accessor, BinaryOperator, Expression, Item, Parameter, Statement, TokenKind, Type,
    TypeDeclarationKind, parse,
};

#[test]
fn parses_hello_world_losslessly() {
    let source = include_str!("../examples/hello_world.sta");
    let root = parse(source).expect("hello_world should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 2);
    assert!(matches!(root.items[0], Item::ExternBlock(_)));
    assert!(matches!(
        root.items[1],
        Item::Statement(Statement::Expression(ref expression))
            if matches!(expression, Expression::Call(_))
    ));
}

#[test]
fn parses_list_parameter_and_expression_body() {
    let source = "def add: _ -> i32 = (a: i32, b: i32) -> a + b\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let Item::Statement(Statement::Binding(binding)) = &root.items[0] else {
        panic!("expected binding");
    };
    let Expression::Function(function) = binding.value.as_ref().expect("function value") else {
        panic!("expected function");
    };
    assert!(matches!(function.parameter, Parameter::List(ref list) if list.elements.len() == 2));
    assert!(matches!(
        *function.body,
        Expression::Binary(ref binary) if binary.operator == BinaryOperator::Add
    ));
}

#[test]
fn parses_single_parameter_and_application() {
    let source = "def println: _ -> i32 = s: string -> printf (\"%s\\n\", s)\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let Item::Statement(Statement::Binding(binding)) = &root.items[0] else {
        panic!("expected binding");
    };
    let Expression::Function(function) = binding.value.as_ref().expect("function value") else {
        panic!("expected function");
    };
    assert!(matches!(function.parameter, Parameter::Value(ref value) if value.name == "s"));
    let Expression::Call(call) = function.body.as_ref() else {
        panic!("expected call");
    };
    assert!(matches!(*call.argument, Expression::List(_)));
}

#[test]
fn comments_and_crlf_are_preserved() {
    let source = "// entry\r\ndef main: _ -> string = () -> {\r\n  \"ok\"\r\n}\r\n";
    let root = parse(source).expect("source should parse");
    assert_eq!(root.text(), source);
    assert!(
        root.syntax
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::LineComment)
    );
    assert!(root.syntax.tokens.iter().any(|token| token.text == "\r\n"));
}

#[test]
fn parses_named_list_types_values_and_access() {
    let source = concat!(
        "let args: (name: string, int)\n",
        "type user_id = int\n",
        "def value = (name: \"staple\", 1)\n",
        "def by_name = args.name\n",
        "def by_index = args.1\n",
    );
    let root = parse(source).expect("named list syntax should parse");

    assert_eq!(root.text(), source);
    let Item::Statement(Statement::Binding(args)) = &root.items[0] else {
        panic!("expected args binding");
    };
    let Type::List(args_type) = args.annotation.as_ref().expect("args type") else {
        panic!("expected list type");
    };
    assert_eq!(args_type.elements[0].name.as_deref(), Some("name"));
    assert_eq!(args_type.elements[1].name, None);

    assert!(matches!(
        root.items[1],
        Item::TypeDeclaration(ref declaration)
            if declaration.kind == TypeDeclarationKind::Distinct
    ));

    let Item::Statement(Statement::Binding(value)) = &root.items[2] else {
        panic!("expected value binding");
    };
    let Expression::List(value) = value.value.as_ref().expect("list value") else {
        panic!("expected list value");
    };
    assert_eq!(value.elements[0].name.as_deref(), Some("name"));

    let Item::Statement(Statement::Binding(by_name)) = &root.items[3] else {
        panic!("expected name access binding");
    };
    assert!(matches!(
        by_name.value,
        Some(Expression::Access(ref access)) if access.accessor == Accessor::Name("name".into())
    ));

    let Item::Statement(Statement::Binding(by_index)) = &root.items[4] else {
        panic!("expected index access binding");
    };
    assert!(matches!(
        by_index.value,
        Some(Expression::Access(ref access)) if access.accessor == Accessor::Index("1".into())
    ));
}

#[test]
fn block_items_are_typed() {
    let source = "def answer = () -> { let x = 40 }\n";
    let root = parse(source).expect("block should parse");
    let Item::Statement(Statement::Binding(binding)) = &root.items[0] else {
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
        Item::Statement(Statement::Binding(_))
    ));
    assert!(matches!(root.items[1], Item::Statement(_)));
    assert!(matches!(root.items[2], Item::Statement(_)));
}
