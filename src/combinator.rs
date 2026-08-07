pub type Result<T> = Option<(T, usize)>;

pub trait Parser<T> {
    fn parse(&self, source: &str, offset: usize) -> Result<T>;

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

pub struct Or<A, B>(A, B);

impl<T, A: Parser<T>, B: Parser<T>> Parser<T> for Or<A, B> {
    fn parse(&self, source: &str, offset: usize) -> Result<T> {
        self.0
            .parse(source, offset)
            .or_else(|| self.1.parse(source, offset))
    }
}

pub fn tag(expected: &'static str) -> impl Parser<&'static str> {
    move |source: &str, offset: usize| {
        source
            .get(offset..)?
            .starts_with(expected)
            .then_some((expected, offset + expected.len()))
    }
}

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
