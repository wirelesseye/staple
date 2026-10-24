use staple_syntax::{Expression, Item, TokenKind, lex, parse};

#[test]
fn character_tokens_are_lossless_and_recover_at_newlines() {
    for source in ["'a'", "'\\''", "'\\u{1F600}'", "'$'", "'é'"] {
        let tokens = lex(source);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Char);
        assert_eq!(tokens[0].text, source);
    }
    for source in ["'a\nlet next = 1", "'\\\r\nlet next = 1"] {
        let tokens = lex(source);
        assert_eq!(tokens[0].kind, TokenKind::Char);
        assert!(tokens.iter().any(|t| t.kind == TokenKind::Let));
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<String>(),
            source
        );
    }
}

#[test]
fn character_expressions_and_template_delimiters() {
    let source = r#"let c = 'é'
let text = "${'}'}${'{'}${'"'}${'\\'}${'\u{2603}'}"
"#;
    let module = parse(source).unwrap();
    assert_eq!(module.syntax.text(), source);
    let Item::Binding(binding) = &module.items[0] else {
        panic!("binding");
    };
    assert!(matches!(&binding.value, Some(Expression::Char(c)) if c.literal == "'é'"));
}
