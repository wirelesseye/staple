/// An invalid character literal, including malformed escapes and scalar counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CharLiteralError {
    Empty,
    MultipleScalars(usize),
    BadEscape(String),
}

impl std::fmt::Display for CharLiteralError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty character literal"),
            Self::MultipleScalars(count) => write!(
                f,
                "character literal contains {count} scalar values; use a String literal instead"
            ),
            Self::BadEscape(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for CharLiteralError {}

fn decode_escape(chars: &mut std::str::Chars<'_>, quote: char) -> Result<char, String> {
    let kind = if quote == '\'' { "character" } else { "string" };
    let escaped = chars
        .next()
        .ok_or_else(|| format!("unterminated {kind} escape"))?;
    Ok(match escaped {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '0' => '\0',
        '\\' => '\\',
        '"' => '"',
        '\'' => '\'',
        '$' if quote == '"' => '$',
        'u' => {
            if chars.next() != Some('{') {
                return Err("expected `{` after `\\u`".to_owned());
            }
            let mut digits = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(c) if c.is_ascii_hexdigit() && digits.len() < 6 => digits.push(c),
                    _ => {
                        return Err(
                            "Unicode escape requires 1 to 6 hex digits followed by `}`".to_owned()
                        );
                    }
                }
            }
            if digits.is_empty() {
                return Err("Unicode escape requires 1 to 6 hex digits".to_owned());
            }
            let value = u32::from_str_radix(&digits, 16).expect("validated hex digits");
            char::from_u32(value)
                .ok_or_else(|| format!("U+{value:04X} is not a Unicode scalar value"))?
        }
        other => return Err(format!("unknown {kind} escape `\\{other}`")),
    })
}

fn decode_content(content: &str, quote: char) -> Result<String, String> {
    let mut output = String::new();
    let mut chars = content.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            output.push(decode_escape(&mut chars, quote)?);
        } else {
            if quote == '"' && c == '$' {
                return Err(
                    "unescaped `$` in string literal; write `\\$` for a literal dollar".to_owned(),
                );
            }
            if c == quote {
                return Err("unescaped quote in literal".to_owned());
            }
            output.push(c);
        }
    }
    Ok(output)
}

pub fn decode(literal: &str) -> Result<String, String> {
    let content = literal
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| "unterminated string literal".to_owned())?;
    decode_content(content, '"')
}

pub fn decode_char(literal: &str) -> Result<char, CharLiteralError> {
    let content = literal
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .ok_or_else(|| CharLiteralError::BadEscape("unterminated character literal".to_owned()))?;
    if content.contains(['\n', '\r']) {
        return Err(CharLiteralError::BadEscape(
            "newline in character literal; use an escape".to_owned(),
        ));
    }
    let decoded = decode_content(content, '\'').map_err(CharLiteralError::BadEscape)?;
    match decoded.chars().count() {
        0 => Err(CharLiteralError::Empty),
        1 => Ok(decoded.chars().next().unwrap()),
        n => Err(CharLiteralError::MultipleScalars(n)),
    }
}

pub fn encode(value: &str) -> String {
    format!("{value:?}").replace('$', "\\$")
}
pub fn encode_char(value: char) -> String {
    format!("{value:?}")
}

/// Decodes literal text between a string template's interpolations.
pub fn decode_template_text(raw: &str) -> Result<String, String> {
    decode_content(raw, '"')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_and_canonically_encodes_literals() {
        for s in ["hello\n", "$5", "é", "\u{301}", "😀", "\0"] {
            assert_eq!(decode(&encode(s)).unwrap(), s);
        }
        assert!(decode("\"$5\"").is_err());
    }
    #[test]
    fn character_literals_and_unicode_escapes() {
        for c in ['a', '\'', '"', '\\', '\0', '\n', 'é', '\u{301}', '😀'] {
            assert_eq!(decode_char(&encode_char(c)), Ok(c));
        }
        assert_eq!(decode_char("'\\u{1F600}'"), Ok('😀'));
        assert_eq!(decode("\"\\u{2603}\\'\"").unwrap(), "☃'");
        assert_eq!(decode_template_text("\\u{2603}").unwrap(), "☃");
        assert_eq!(decode_char("''"), Err(CharLiteralError::Empty));
        assert_eq!(
            decode_char("'e\u{301}'"),
            Err(CharLiteralError::MultipleScalars(2))
        );
        assert_eq!(
            decode_char("'🇦🇺'"),
            Err(CharLiteralError::MultipleScalars(2))
        );
        for literal in [
            "'\\u{D800}'",
            "'\\u{110000}'",
            "'\\u{}'",
            "'\\u{1234567}'",
            "'\\u{xyz}'",
            "'\\q'",
            "'\\$'",
            "'a",
        ] {
            assert!(decode_char(literal).is_err(), "{literal}");
        }
    }
}
