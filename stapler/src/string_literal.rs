pub(crate) fn decode(literal: &str) -> Result<String, String> {
    let content = literal
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| "unterminated string literal".to_owned())?;
    let mut output = String::new();
    let mut characters = content.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let escaped = characters
            .next()
            .ok_or_else(|| "unterminated string escape".to_owned())?;
        output.push(match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            '\\' => '\\',
            '"' => '"',
            other => return Err(format!("unknown string escape `\\{other}`")),
        });
    }
    Ok(output)
}

pub(crate) fn encode(value: &str) -> String {
    format!("{value:?}")
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn decodes_and_canonically_encodes_literals() {
        assert_eq!(decode("\"hello\\n\"").unwrap(), "hello\n");
        assert_eq!(encode("hello\n"), "\"hello\\n\"");
    }
}
