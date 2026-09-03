/// A successful parse result containing the parsed value and ending byte offset.
///
/// `None` indicates that the parser did not match at the requested offset.
pub type Result<T> = Option<(T, usize)>;

/// A parser that attempts to read a value from a source byte offset.
pub trait Parser<T> {
    /// Parses `source` beginning at `offset`.
    ///
    /// A successful result includes the first unconsumed byte offset. Implementors
    /// should return `None` when the source does not match at the starting offset.
    fn parse(&self, source: &str, offset: usize) -> Result<T>;

    /// Transforms this parser's output without changing how much input it consumes.
    fn map<U, F>(self, map: F) -> Map<Self, F, T>
    where
        Self: Sized,
        F: Fn(T) -> U,
    {
        Map {
            parser: self,
            map,
            _input: std::marker::PhantomData,
        }
    }

    /// Tries this parser, then `other` from the same offset if it does not match.
    fn or<P>(self, other: P) -> Or<Self, P>
    where
        Self: Sized,
        P: Parser<T>,
    {
        Or(self, other)
    }
}

impl<T, F> Parser<T> for F
where
    F: Fn(&str, usize) -> Result<T>,
{
    fn parse(&self, source: &str, offset: usize) -> Result<T> {
        self(source, offset)
    }
}

/// A parser that transforms the value produced by another parser.
pub struct Map<P, F, T> {
    parser: P,
    map: F,
    _input: std::marker::PhantomData<T>,
}

impl<T, U, P, F> Parser<U> for Map<P, F, T>
where
    P: Parser<T>,
    F: Fn(T) -> U,
{
    fn parse(&self, source: &str, offset: usize) -> Result<U> {
        self.parser
            .parse(source, offset)
            .map(|(value, end)| ((self.map)(value), end))
    }
}

/// A parser that falls back to a second parser when the first does not match.
pub struct Or<A, B>(A, B);

impl<T, A: Parser<T>, B: Parser<T>> Parser<T> for Or<A, B> {
    fn parse(&self, source: &str, offset: usize) -> Result<T> {
        self.0
            .parse(source, offset)
            .or_else(|| self.1.parse(source, offset))
    }
}

/// Returns a parser that matches `expected` exactly at the starting offset.
///
/// The parser produces `expected` and advances by its UTF-8 byte length.
pub fn tag(expected: &'static str) -> impl Parser<&'static str> {
    move |source: &str, offset: usize| {
        source
            .get(offset..)?
            .starts_with(expected)
            .then_some((expected, offset + expected.len()))
    }
}

/// Returns a parser that consumes one or more characters matching `predicate`.
///
/// The parser produces `()` and reports its ending UTF-8 byte offset. It does not
/// match when the first character fails the predicate or the offset is invalid.
pub fn take_while1(predicate: impl Fn(char) -> bool) -> impl Parser<()> {
    move |source: &str, offset: usize| {
        let tail = source.get(offset..)?;
        let mut length = 0;
        for character in tail.chars() {
            if !predicate(character) {
                break;
            }
            length += character.len_utf8();
        }
        (length > 0).then_some(((), offset + length))
    }
}
