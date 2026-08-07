use stapler::{
    Accessor, BinaryOperator, BlockItem, Expression, Item, Parameter, TokenKind, Type,
    TypeDeclarationKind, parse,
};

#[test]
fn parses_hello_world_losslessly() {
    let source = include_str!("../examples/hello_world.sta");
    let root = parse(source).expect("hello_world should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.items.len(), 3);
    assert!(matches!(root.items[0], Item::ExternBlock(_)));
    assert!(matches!(
        root.items[1],
        Item::TypeDeclaration(ref declaration)
            if declaration.kind == TypeDeclarationKind::Alias
    ));

    let Item::Binding(main) = &root.items[2] else {
        panic!("main should be a binding");
    };
    let Type::Function(function_type) = main.annotation.as_ref().expect("main type") else {
        panic!("main should have a function type");
    };
    assert!(matches!(*function_type.parameter, Type::Inferred(_)));
    assert!(
        function_type
            .syntax
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::Underscore)
    );

    let Expression::Function(function) = main.value.as_ref().expect("main value") else {
        panic!("main should contain a function value");
    };
    assert!(matches!(function.parameter, Parameter::List(ref list) if list.elements.is_empty()));
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("main should have a block body");
    };
    assert!(matches!(block.items[0], BlockItem::Expression(_)));
}

#[test]
fn parses_list_parameter_and_expression_body() {
    let source = "def add: _ -> i32 = (a: i32, b: i32) -> a + b\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let Item::Binding(binding) = &root.items[0] else {
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
    let Item::Binding(binding) = &root.items[0] else {
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
fn arrow_is_always_followed_by_the_function_body() {
    let source = "def main = () -> i32 { 0 }\n";
    let root = parse(source).expect("this is a function whose body calls `i32`");
    let Item::Binding(binding) = &root.items[0] else {
        panic!("expected binding");
    };
    let Expression::Function(function) = binding.value.as_ref().expect("function value") else {
        panic!("expected function");
    };
    assert!(matches!(*function.body, Expression::Call(_)));
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
    let Item::Binding(args) = &root.items[0] else {
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

    let Item::Binding(value) = &root.items[2] else {
        panic!("expected value binding");
    };
    let Expression::List(value) = value.value.as_ref().expect("list value") else {
        panic!("expected list value");
    };
    assert_eq!(value.elements[0].name.as_deref(), Some("name"));

    let Item::Binding(by_name) = &root.items[3] else {
        panic!("expected name access binding");
    };
    assert!(matches!(
        by_name.value,
        Some(Expression::Access(ref access)) if access.accessor == Accessor::Name("name".into())
    ));

    let Item::Binding(by_index) = &root.items[4] else {
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
    let Item::Binding(binding) = &root.items[0] else {
        panic!("expected binding");
    };
    let Some(Expression::Function(function)) = &binding.value else {
        panic!("expected function");
    };
    let Expression::Block(block) = function.body.as_ref() else {
        panic!("expected block");
    };
    assert!(matches!(block.items[0], BlockItem::Binding(_)));
}
