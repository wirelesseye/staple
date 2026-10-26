//! The meaning of numeric literal text.
//!
//! The lexer keeps every number literal losslessly, including malformed ones;
//! this module is the single place that decides what a literal denotes. It is
//! parallel to [`crate::string_literal`] and is shared by the checker, the
//! lowerer, compile-time evaluation and editor tooling.

use std::fmt;

/// The base of an integer literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Radix {
    Binary,
    Octal,
    Decimal,
    Hexadecimal,
}

impl Radix {
    pub fn name(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Octal => "octal",
            Self::Decimal => "decimal",
            Self::Hexadecimal => "hexadecimal",
        }
    }

    /// The lowercase prefix that introduces this radix; the empty string for
    /// decimal.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Binary => "0b",
            Self::Octal => "0o",
            Self::Decimal => "",
            Self::Hexadecimal => "0x",
        }
    }

    fn is_digit(self, character: char) -> bool {
        match self {
            Self::Binary => matches!(character, '0' | '1'),
            Self::Octal => ('0'..='7').contains(&character),
            Self::Decimal => character.is_ascii_digit(),
            Self::Hexadecimal => character.is_ascii_hexdigit(),
        }
    }
}

/// A literal type suffix. Suffixes are lowercase type names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumberSuffix {
    I8,
    I16,
    I32,
    I64,
    ISize,
    U8,
    U16,
    U32,
    U64,
    USize,
    F32,
    F64,
}

impl NumberSuffix {
    pub const ALL: [Self; 12] = [
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::ISize,
        Self::U8,
        Self::U16,
        Self::U32,
        Self::U64,
        Self::USize,
        Self::F32,
        Self::F64,
    ];

    /// The suffix exactly as written after a literal.
    pub fn spelling(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::ISize => "isize",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::USize => "usize",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    pub fn is_integer(self) -> bool {
        !self.is_float()
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|suffix| suffix.spelling() == text)
    }
}

/// A decoded integer literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerLiteral {
    /// The magnitude. `u128` so callers can range-check any 64-bit type and
    /// still tell "too large for U64" apart from a decode error.
    pub value: u128,
    pub suffix: Option<NumberSuffix>,
    pub radix: Radix,
}

/// A decoded float literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatLiteral {
    /// The digits with separators and suffix removed, ready for
    /// `str::parse::<f32/f64>`.
    pub digits: String,
    pub suffix: Option<NumberSuffix>,
}

/// Why a number literal could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumberLiteralError {
    MissingDigits {
        radix: Radix,
    },
    InvalidDigit {
        digit: char,
        radix: Radix,
    },
    UppercasePrefix {
        prefix: String,
    },
    MisplacedSeparator,
    UnknownSuffix {
        suffix: String,
    },
    UppercaseSuffix {
        suffix: String,
    },
    FloatSuffixOnRadix {
        radix: Radix,
    },
    /// `decode_integer` was given a token the lexer made `Float`.
    FloatSuffixOnInteger,
    IntegerSuffixOnFloat,
    TooLarge,
}

impl fmt::Display for NumberLiteralError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDigits { radix } => {
                write!(f, "{} literal is missing digits", radix.name())
            }
            Self::InvalidDigit { digit, radix } => {
                write!(f, "invalid digit `{digit}` in {} literal", radix.name())
            }
            Self::UppercasePrefix { prefix } => {
                let lowercase = prefix.to_ascii_lowercase();
                write!(
                    f,
                    "invalid number prefix `{prefix}`; write `{lowercase}` for a {} literal",
                    prefix_radix(prefix).name()
                )
            }
            Self::MisplacedSeparator => {
                f.write_str("misplaced `_` digit separator in number literal")
            }
            Self::UnknownSuffix { suffix } => {
                write!(f, "invalid suffix `{suffix}` on number literal")
            }
            Self::UppercaseSuffix { suffix } => {
                let lowercase = suffix.to_ascii_lowercase();
                write!(
                    f,
                    "invalid suffix `{suffix}` on number literal; did you mean `{lowercase}`?"
                )
            }
            Self::FloatSuffixOnRadix { radix } => {
                write!(f, "a {} literal cannot be a float", radix.name())
            }
            Self::FloatSuffixOnInteger => {
                f.write_str("expected an integer literal, found a float literal")
            }
            Self::IntegerSuffixOnFloat => {
                f.write_str("an integer suffix cannot be used on a float literal")
            }
            Self::TooLarge => f.write_str("number literal is too large"),
        }
    }
}
impl std::error::Error for NumberLiteralError {}

fn prefix_radix(prefix: &str) -> Radix {
    match prefix.as_bytes().get(1) {
        Some(b'X' | b'x') => Radix::Hexadecimal,
        Some(b'B' | b'b') => Radix::Binary,
        Some(b'O' | b'o') => Radix::Octal,
        _ => Radix::Decimal,
    }
}

/// Splits a lowercase radix prefix from `literal`. Uppercase prefixes are
/// rejected with a fix-it rather than treated as an identifier suffix.
fn split_prefix(literal: &str) -> Result<(Radix, usize), NumberLiteralError> {
    let bytes = literal.as_bytes();
    if bytes.first() == Some(&b'0') {
        match bytes.get(1) {
            Some(b'x') => return Ok((Radix::Hexadecimal, 2)),
            Some(b'b') => return Ok((Radix::Binary, 2)),
            Some(b'o') => return Ok((Radix::Octal, 2)),
            Some(b'X' | b'B' | b'O') => {
                return Err(NumberLiteralError::UppercasePrefix {
                    prefix: literal[..2].to_owned(),
                });
            }
            _ => {}
        }
    }
    Ok((Radix::Decimal, 0))
}

fn is_suffix_start(character: char) -> bool {
    matches!(character, 'i' | 'u' | 'f' | 'I' | 'U' | 'F')
}

fn parse_decimal_suffix(text: &str) -> Result<NumberSuffix, NumberLiteralError> {
    if let Some(suffix) = NumberSuffix::parse(text) {
        return Ok(suffix);
    }
    let lowercase = text.to_ascii_lowercase();
    if text.chars().any(|character| character.is_ascii_uppercase())
        && NumberSuffix::parse(&lowercase).is_some()
    {
        return Err(NumberLiteralError::UppercaseSuffix {
            suffix: text.to_owned(),
        });
    }
    Err(NumberLiteralError::UnknownSuffix {
        suffix: text.to_owned(),
    })
}

fn accumulate(value: &mut u128, radix: Radix, character: char) -> Result<(), NumberLiteralError> {
    let digit = character
        .to_digit(16)
        .ok_or(NumberLiteralError::InvalidDigit {
            digit: character,
            radix,
        })?;
    let radix = match radix {
        Radix::Binary => 2,
        Radix::Octal => 8,
        Radix::Decimal => 10,
        Radix::Hexadecimal => 16,
    };
    *value = value
        .checked_mul(radix)
        .and_then(|value| value.checked_add(u128::from(digit)))
        .ok_or(NumberLiteralError::TooLarge)?;
    Ok(())
}

/// Decodes an integer literal with an optional radix prefix and type suffix.
pub fn decode_integer(literal: &str) -> Result<IntegerLiteral, NumberLiteralError> {
    let (radix, start) = split_prefix(literal)?;
    let body = &literal[start..];
    let mut value = 0_u128;
    let mut saw_digit = false;
    let mut characters = body.char_indices().peekable();

    while let Some((offset, character)) = characters.next() {
        if radix.is_digit(character) {
            saw_digit = true;
            accumulate(&mut value, radix, character)?;
            continue;
        }
        if character == '_' {
            let previous = body[..offset].chars().next_back();
            let next = body[offset + 1..].chars().next();
            let previous_is_digit = previous.is_some_and(|previous| radix.is_digit(previous));
            let next_is_valid =
                next.is_some_and(|next| radix.is_digit(next) || is_suffix_start(next));
            if !previous_is_digit || !next_is_valid {
                return Err(NumberLiteralError::MisplacedSeparator);
            }
            continue;
        }
        if !saw_digit {
            // `0bu8` has a suffix but no digits; report the missing digits
            // rather than blaming the `u`.
            let suffix = &body[offset..];
            if NumberSuffix::parse(suffix).is_some() {
                return Err(NumberLiteralError::MissingDigits { radix });
            }
            return Err(NumberLiteralError::InvalidDigit {
                digit: character,
                radix,
            });
        }
        let suffix = &body[offset..];
        if radix == Radix::Decimal {
            if matches!(character, '.' | 'e' | 'E') {
                return Err(NumberLiteralError::FloatSuffixOnInteger);
            }
            let suffix = parse_decimal_suffix(suffix)?;
            if suffix.is_float() {
                return Err(NumberLiteralError::FloatSuffixOnInteger);
            }
            return Ok(IntegerLiteral {
                value,
                suffix: Some(suffix),
                radix,
            });
        }
        // In a prefixed literal a character outside the radix is either a
        // suffix or a bad digit. Prefer the suffix reading when the run is an
        // exact (possibly uppercase) type name.
        if !NumberSuffix::parse(suffix).is_some()
            && !(suffix
                .chars()
                .any(|character| character.is_ascii_uppercase())
                && NumberSuffix::parse(&suffix.to_ascii_lowercase()).is_some())
        {
            return Err(NumberLiteralError::InvalidDigit {
                digit: character,
                radix,
            });
        }
        let suffix = parse_decimal_suffix(suffix)?;
        if suffix.is_float() {
            return Err(NumberLiteralError::FloatSuffixOnRadix { radix });
        }
        return Ok(IntegerLiteral {
            value,
            suffix: Some(suffix),
            radix,
        });
    }

    if !saw_digit {
        return Err(NumberLiteralError::MissingDigits { radix });
    }
    Ok(IntegerLiteral {
        value,
        suffix: None,
        radix,
    })
}

/// Decodes a float literal. Floats are always decimal; a float suffix on a
/// prefixed integer literal is an error.
pub fn decode_float(literal: &str) -> Result<FloatLiteral, NumberLiteralError> {
    let (radix, start) = split_prefix(literal)?;
    if radix != Radix::Decimal {
        return Err(NumberLiteralError::FloatSuffixOnRadix { radix });
    }
    // Reified constants can carry a leading `-`, even though source literals
    // cannot; keep it in `digits` so `str::parse` sees it.
    let (sign, unsigned) = match literal.strip_prefix('-') {
        Some(unsigned) => ("-", unsigned),
        None => ("", &literal[start..]),
    };
    let text = unsigned;
    let bytes = text.as_bytes();
    let mut end = 0;

    while bytes
        .get(end)
        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
    {
        end += 1;
    }
    let mut saw_digit = end > 0;
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        while bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
        {
            saw_digit |= bytes[end].is_ascii_digit();
            end += 1;
        }
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let exponent = end;
        let mut exponent_end = end + 1;
        if matches!(bytes.get(exponent_end), Some(b'+' | b'-')) {
            exponent_end += 1;
        }
        let digits_start = exponent_end;
        while bytes
            .get(exponent_end)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
        {
            exponent_end += 1;
        }
        let next_is_identifier = text[exponent_end..]
            .chars()
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
        if exponent_end > digits_start || !next_is_identifier {
            end = exponent_end.max(exponent + 1);
        }
    }
    if !saw_digit {
        return Err(NumberLiteralError::MissingDigits { radix });
    }

    let numeric = &text[..end];
    for (offset, character) in numeric.char_indices() {
        if character != '_' {
            continue;
        }
        let previous = numeric[..offset].chars().next_back();
        let next = text[offset + 1..].chars().next();
        let next_is_valid = next.is_some_and(|next| {
            next.is_ascii_digit() || (offset + 1 == numeric.len() && is_suffix_start(next))
        });
        if !previous.is_some_and(|previous| previous.is_ascii_digit()) || !next_is_valid {
            return Err(NumberLiteralError::MisplacedSeparator);
        }
    }

    let digits = format!(
        "{sign}{}",
        numeric
            .chars()
            .filter(|character| *character != '_')
            .collect::<String>()
    );
    let suffix_text = &text[end..];
    let suffix = if suffix_text.is_empty() {
        None
    } else {
        let suffix = parse_decimal_suffix(suffix_text)?;
        if suffix.is_integer() {
            return Err(NumberLiteralError::IntegerSuffixOnFloat);
        }
        Some(suffix)
    };
    Ok(FloatLiteral { digits, suffix })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integer(literal: &str) -> IntegerLiteral {
        decode_integer(literal).unwrap_or_else(|error| panic!("{literal}: {error}"))
    }

    fn float(literal: &str) -> FloatLiteral {
        decode_float(literal).unwrap_or_else(|error| panic!("{literal}: {error}"))
    }

    #[test]
    fn decodes_radixes_and_suffixes() {
        assert_eq!(
            integer("0"),
            IntegerLiteral {
                value: 0,
                suffix: None,
                radix: Radix::Decimal
            }
        );
        assert_eq!(integer("1_0").value, 10, "separators carry no meaning");
        assert_eq!(integer("0xFF").value, 255);
        assert_eq!(integer("0xff").value, 255);
        assert_eq!(integer("0xFf").value, 255);
        assert_eq!(integer("0b1010").value, 10);
        assert_eq!(integer("0o755").value, 493);
        assert_eq!(integer("0755").value, 755);
        assert_eq!(integer("0755").radix, Radix::Decimal);
        for suffix in NumberSuffix::ALL
            .into_iter()
            .filter(|suffix| suffix.is_integer())
        {
            assert_eq!(
                integer(&format!("42{}", suffix.spelling())).suffix,
                Some(suffix)
            );
        }
        assert_eq!(integer("0xFFu8").suffix, Some(NumberSuffix::U8));
        assert_eq!(integer("0b1010_u16").suffix, Some(NumberSuffix::U16));
        assert_eq!(integer("0o755i16").suffix, Some(NumberSuffix::I16));
        assert_eq!(integer("0x7F_i8").suffix, Some(NumberSuffix::I8));
        assert_eq!(integer("0x7F_i8").value, 0x7F);
    }

    #[test]
    fn decodes_large_values_and_reports_overflow() {
        assert_eq!(integer("0xFFFF_FFFF_FFFF_FFFF").value, u128::from(u64::MAX));
        assert_eq!(
            integer(
                "0b1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111"
            )
            .value,
            u128::from(u64::MAX)
        );
        assert_eq!(
            integer("0o1_777_777_777_777_777_777_777").value,
            u128::from(u64::MAX)
        );
        assert_eq!(
            integer("18_446_744_073_709_551_615").value,
            u128::from(u64::MAX)
        );
        assert_eq!(
            integer("0x1_0000_0000_0000_0000").value,
            u128::from(u64::MAX) + 1
        );
        assert_eq!(
            integer("18_446_744_073_709_551_616").value,
            u128::from(u64::MAX) + 1
        );
        assert_eq!(
            decode_integer("0x1_0000_0000_0000_0000_0000_0000_0000_0000"),
            Err(NumberLiteralError::TooLarge)
        );
    }

    #[test]
    fn decodes_float_separators_and_suffixes() {
        assert_eq!(float("1_000.000_1").digits, "1000.0001");
        assert_eq!(float("6.022e2_3").digits, "6.022e23");
        assert_eq!(float("1e1_0").digits, "1e10");
        assert_eq!(float("1_000.5").digits, "1000.5");
        assert_eq!(float("1_000.5").suffix, None);
        assert_eq!(float("1e3f32").digits, "1e3");
        assert_eq!(float("1e3f32").suffix, Some(NumberSuffix::F32));
        assert_eq!(float("1.5f32").suffix, Some(NumberSuffix::F32));
        assert_eq!(float("7f64").suffix, Some(NumberSuffix::F64));
        assert_eq!(float("1_f32").suffix, Some(NumberSuffix::F32));
        assert_eq!(float(".5").digits, ".5");
        assert_eq!(float(".5e-3").digits, ".5e-3");
        assert_eq!(float("1.").digits, "1.");
        assert_eq!(
            float("-1.7976931348623157e308").digits,
            "-1.7976931348623157e308"
        );
    }

    #[test]
    fn rejects_malformed_integer_literals() {
        for (literal, expected) in [
            (
                "0x",
                NumberLiteralError::MissingDigits {
                    radix: Radix::Hexadecimal,
                },
            ),
            (
                "0b",
                NumberLiteralError::MissingDigits {
                    radix: Radix::Binary,
                },
            ),
            (
                "0o",
                NumberLiteralError::MissingDigits {
                    radix: Radix::Octal,
                },
            ),
            (
                "0bu8",
                NumberLiteralError::MissingDigits {
                    radix: Radix::Binary,
                },
            ),
            (
                "0xG",
                NumberLiteralError::InvalidDigit {
                    digit: 'G',
                    radix: Radix::Hexadecimal,
                },
            ),
            (
                "0b2",
                NumberLiteralError::InvalidDigit {
                    digit: '2',
                    radix: Radix::Binary,
                },
            ),
            (
                "0o8",
                NumberLiteralError::InvalidDigit {
                    digit: '8',
                    radix: Radix::Octal,
                },
            ),
            (
                "0X1",
                NumberLiteralError::UppercasePrefix {
                    prefix: "0X".into(),
                },
            ),
            (
                "0B1",
                NumberLiteralError::UppercasePrefix {
                    prefix: "0B".into(),
                },
            ),
            ("0x_1", NumberLiteralError::MisplacedSeparator),
            ("1_", NumberLiteralError::MisplacedSeparator),
            ("1__0", NumberLiteralError::MisplacedSeparator),
            (
                "42U8",
                NumberLiteralError::UppercaseSuffix {
                    suffix: "U8".into(),
                },
            ),
            (
                "42abc",
                NumberLiteralError::UnknownSuffix {
                    suffix: "abc".into(),
                },
            ),
            (
                "0b1f32",
                NumberLiteralError::FloatSuffixOnRadix {
                    radix: Radix::Binary,
                },
            ),
            (
                "0o7f64",
                NumberLiteralError::FloatSuffixOnRadix {
                    radix: Radix::Octal,
                },
            ),
        ] {
            assert_eq!(decode_integer(literal), Err(expected), "{literal}");
        }
    }

    #[test]
    fn rejects_malformed_float_literals() {
        for (literal, expected) in [
            ("1_.5", NumberLiteralError::MisplacedSeparator),
            ("1.5_e3", NumberLiteralError::MisplacedSeparator),
            (
                "0b1f32",
                NumberLiteralError::FloatSuffixOnRadix {
                    radix: Radix::Binary,
                },
            ),
            ("1.5u8", NumberLiteralError::IntegerSuffixOnFloat),
            (
                "42U8",
                NumberLiteralError::UppercaseSuffix {
                    suffix: "U8".into(),
                },
            ),
            (
                "42abc",
                NumberLiteralError::UnknownSuffix {
                    suffix: "abc".into(),
                },
            ),
        ] {
            assert_eq!(decode_float(literal), Err(expected), "{literal}");
        }
    }
}
