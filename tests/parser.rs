use stapler::{NodeKind, TokenKind, parse};

#[test]
fn parses_hello_world_losslessly() {
    let source = include_str!("../examples/hello_world.sta");
    let root = parse(source).expect("hello_world should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.kind, NodeKind::SourceFile);
    assert_eq!(root.children.len(), 3);
    assert_eq!(root.children[0].kind, NodeKind::ExternBlock);
    assert_eq!(root.children[1].kind, NodeKind::TypeAlias);
    assert_eq!(root.children[2].kind, NodeKind::DefBinding);

    let binding_type = &root.children[2].children[0];
    let function = &root.children[2].children[1];
    assert_eq!(binding_type.kind, NodeKind::Type);
    assert!(
        binding_type
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::Underscore)
    );
    assert_eq!(function.kind, NodeKind::FunctionValue);
    assert_eq!(function.children[0].kind, NodeKind::Parameter);
    assert_eq!(function.children[1].kind, NodeKind::BlockExpression);
}

#[test]
fn parses_list_parameter_and_expression_body() {
    let source = "def add: _ -> i32 = (a: i32, b: i32) -> a + b\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let function = &root.children[0].children[1];
    assert_eq!(function.kind, NodeKind::FunctionValue);
    assert_eq!(function.children[0].kind, NodeKind::Parameter);
    assert_eq!(function.children[1].kind, NodeKind::BinaryExpression);
}

#[test]
fn parses_single_parameter_and_application() {
    let source = "def println: _ -> i32 = s: string -> printf (\"%s\\n\", s)\n";
    let root = parse(source).expect("function should parse");
    assert_eq!(root.text(), source);
    let body = &root.children[0].children[1].children[1];
    assert_eq!(body.kind, NodeKind::CallExpression);
    assert_eq!(body.children[1].kind, NodeKind::ListExpression);
}

#[test]
fn comments_and_crlf_are_preserved() {
    let source = "// entry\r\ndef main: _ -> string = () -> {\r\n  \"ok\"\r\n}\r\n";
    let root = parse(source).expect("source should parse");
    assert_eq!(root.text(), source);
    assert!(
        root.tokens
            .iter()
            .any(|token| token.kind == TokenKind::LineComment)
    );
    assert!(root.tokens.iter().any(|token| token.text == "\r\n"));
}

#[test]
fn arrow_is_always_followed_by_the_function_body() {
    let source = "def main = () -> i32 { 0 }\n";
    let root = parse(source).expect("this is a function whose body calls `i32`");
    let function = &root.children[0].children[0];
    assert_eq!(function.kind, NodeKind::FunctionValue);
    assert_eq!(function.children.len(), 2);
    assert_eq!(function.children[1].kind, NodeKind::CallExpression);
}

#[test]
fn parses_named_list_types_values_and_access() {
    let source = concat!(
        "let args: (name: string, int)\n",
        "def value = (name: \"staple\", 1)\n",
        "def by_name = args.name\n",
        "def by_index = args.1\n",
    );
    let root = parse(source).expect("named list syntax should parse");

    assert_eq!(root.text(), source);
    assert_eq!(root.children[0].children[0].kind, NodeKind::Type);
    assert_eq!(
        root.children[1].children[0].children[0].kind,
        NodeKind::NamedListElement
    );
    assert_eq!(
        root.children[2].children[0].kind,
        NodeKind::AccessExpression
    );
    assert_eq!(
        root.children[3].children[0].kind,
        NodeKind::AccessExpression
    );
}
