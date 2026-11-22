//! The byte encoding of macro calls and their results.
//!
//! Compiled macro code may run in another process. Everything a call needs
//! crosses as one message: the parent sends a [`Message::Call`] with the
//! call's context and its matched arguments, and gets back a
//! [`Message::Result`] or a [`Message::Error`]. Nothing here knows how the
//! bytes travel.
//!
//! Syntax crosses as token fragments, not as trees. A fragment carries its
//! tokens, each with its kind, text, span, origin and hygiene, and the
//! fragment's own span, definition module, expansion mark and identifier
//! origins. A fragment that is a slice of a longer stream, such as an argument
//! cut from its file, also carries where in that stream it starts, which the
//! receiver rebuilds as trivia (see [`stream_prefix`]). The receiver parses a
//! fragment when it needs a node, and the
//! parent reads a result by the macro's declared category, so a node's id and
//! its `pipe_stage` and `implied` flags don't cross: the receiver numbers the
//! nodes it parses.
//!
//! # Layout
//!
//! A message is a frame:
//!
//! ```text
//! frame   = length:u32le body            length counts the bytes of body
//! body    = version:u32le tag:u8 strings payload
//! strings = count string*                each string is length bytes(UTF-8)
//! ```
//!
//! Every other integer is an unsigned LEB128 varint, and a signed one is
//! zigzag-encoded first. Texts, source names and identifier names are indices
//! into the frame's string table, so each distinct string crosses once. The
//! payload of each tag is described where it is written. Changing any of it,
//! including the order of [`TOKEN_KINDS`], needs a new [`WIRE_VERSION`].

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use staple_syntax::{SourceLocation, Span, Syntax, SyntaxId, SyntaxToken, TokenKind};

use super::args::ArgTree;
use super::quote::CallContext;
use crate::ModuleId;

/// The version every frame starts with. A receiver rejects any other.
pub(crate) const WIRE_VERSION: u32 = 2;

const CALL: u8 = 0;
const RESULT: u8 = 1;
const ERROR: u8 = 2;

const NODE: u8 = 0;
const LIST: u8 = 1;
const ABSENT: u8 = 2;
const GROUP: u8 = 3;

const SPAN_USER: u8 = 1;
const SPAN_SOURCE: u8 = 2;
const SPAN_LOCATION: u8 = 4;

const TOKEN_ORIGIN: u8 = 1;
const TOKEN_HYGIENE: u8 = 2;

const FRAGMENT_MODULE: u8 = 1;
const FRAGMENT_MARK: u8 = 2;

macro_rules! token_kinds {
    ($($kind:ident),* $(,)?) => {
        /// Every token kind, in the order of its code on the wire.
        pub(crate) const TOKEN_KINDS: &[TokenKind] = &[$(TokenKind::$kind),*];

        /// Fails to compile when a token kind is missing from [`TOKEN_KINDS`].
        #[allow(dead_code)]
        fn every_kind_has_a_code(kind: TokenKind) {
            match kind {
                $(TokenKind::$kind)|* => {}
            }
        }
    };
}

token_kinds!(
    Use,
    Package,
    As,
    Pub,
    Let,
    Mut,
    Move,
    Signal,
    Return,
    Loop,
    Break,
    Continue,
    Def,
    Const,
    Extern,
    Type,
    Mod,
    Companion,
    Macro,
    Trait,
    Impl,
    Match,
    Where,
    Resource,
    With,
    Underscore,
    Identifier,
    String,
    Char,
    Byte,
    Integer,
    Float,
    Whitespace,
    Newline,
    LineComment,
    BlockComment,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    DoubleColon,
    Comma,
    Semicolon,
    Caret,
    Dot,
    Equals,
    Arrow,
    FatArrow,
    Ellipsis,
    Dollar,
    At,
    Bang,
    Operator,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Unknown,
);

/// One message between the parent and the process that runs compiled macro
/// code.
#[derive(Clone, Debug)]
pub(crate) enum Message {
    /// The parent asks for one macro call.
    Call {
        /// The symbol of the macro's entry function.
        entry: String,
        context: CallContext,
        /// One tree per parameter, as the parent matched them.
        arguments: Vec<ArgTree>,
    },
    /// A call returned.
    Result {
        /// What the entry function returned, before the parent reads it by
        /// the macro's result category.
        syntax: Syntax,
        /// The first expansion mark the call didn't use, which the parent
        /// continues from.
        next_mark: u64,
    },
    /// A call failed, with the message the macro or the runtime gave.
    Error { message: String },
}

/// Why bytes are not a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WireError {
    /// The bytes end before the frame or a field does.
    Truncated,
    /// The frame was written for another version.
    Version(u32),
    /// A tag, token kind or flag set has no meaning.
    Invalid(&'static str, u64),
    /// A varint doesn't fit the field it encodes.
    Overflow,
    /// A string isn't UTF-8.
    Utf8,
    /// The frame's payload ends before its length says.
    Trailing(usize),
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(formatter, "the message ends early"),
            Self::Version(version) => write!(
                formatter,
                "the message has wire version {version}, but this compiler reads {WIRE_VERSION}"
            ),
            Self::Invalid(what, value) => write!(formatter, "{value} is not a valid {what}"),
            Self::Overflow => write!(formatter, "a number in the message is too large"),
            Self::Utf8 => write!(formatter, "a string in the message is not UTF-8"),
            Self::Trailing(count) => {
                write!(formatter, "the message has {count} bytes after its payload")
            }
        }
    }
}

/// Encodes `message` as one frame.
#[allow(dead_code)] // Nothing outside tests sends messages yet.
pub(crate) fn encode(message: &Message) -> Vec<u8> {
    let mut writer = Writer::default();
    let tag = match message {
        Message::Call {
            entry,
            context,
            arguments,
        } => {
            // entry context arguments(count tree*)
            writer.string(entry);
            writer.context(context);
            writer.varint(arguments.len() as u64);
            for argument in arguments {
                writer.tree(argument);
            }
            CALL
        }
        Message::Result { syntax, next_mark } => {
            // fragment next_mark
            writer.fragment(syntax);
            writer.varint(*next_mark);
            RESULT
        }
        Message::Error { message } => {
            // message
            writer.string(message);
            ERROR
        }
    };
    let mut body = Vec::with_capacity(writer.payload.len() + 64);
    body.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    body.push(tag);
    put_varint(&mut body, writer.strings.len() as u64);
    for string in &writer.strings {
        put_varint(&mut body, string.len() as u64);
        body.extend_from_slice(string.as_bytes());
    }
    body.extend_from_slice(&writer.payload);
    let length = u32::try_from(body.len()).expect("a message fits in 4 GiB");
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// Decodes the frame at the start of `bytes`, and returns it with the bytes
/// after it.
#[allow(dead_code)] // Nothing outside tests receives messages yet.
pub(crate) fn decode(bytes: &[u8]) -> Result<(Message, &[u8]), WireError> {
    let (length, rest) = split(bytes, 4)?;
    let length = u32::from_le_bytes(length.try_into().expect("four bytes")) as usize;
    let (body, rest) = split(rest, length)?;
    let mut reader = Reader {
        bytes: body,
        strings: Vec::new(),
    };
    let version = u32::from_le_bytes(reader.take(4)?.try_into().expect("four bytes"));
    if version != WIRE_VERSION {
        return Err(WireError::Version(version));
    }
    let tag = reader.byte()?;
    let count = reader.length()?;
    let mut strings = Vec::with_capacity(count.min(body.len()));
    for _ in 0..count {
        let length = reader.length()?;
        let text = std::str::from_utf8(reader.take(length)?).map_err(|_| WireError::Utf8)?;
        strings.push(Arc::<str>::from(text));
    }
    reader.strings = strings;
    let message = match tag {
        CALL => {
            let entry = reader.string()?.to_string();
            let context = reader.context()?;
            let count = reader.length()?;
            let mut arguments = Vec::with_capacity(count.min(body.len()));
            for _ in 0..count {
                arguments.push(reader.tree()?);
            }
            Message::Call {
                entry,
                context,
                arguments,
            }
        }
        RESULT => {
            let syntax = reader.fragment()?;
            let next_mark = reader.varint()?;
            Message::Result { syntax, next_mark }
        }
        ERROR => Message::Error {
            message: reader.string()?.to_string(),
        },
        tag => return Err(WireError::Invalid("message tag", u64::from(tag))),
    };
    if !reader.bytes.is_empty() {
        return Err(WireError::Trailing(reader.bytes.len()));
    }
    Ok((message, rest))
}

/// Encodes one syntax fragment with [`WIRE_VERSION`] and its string table, for
/// manifests and template tables.
pub(crate) fn encode_fragment(syntax: &Syntax) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.fragment(syntax);
    let mut bytes = Vec::with_capacity(writer.payload.len() + 32);
    bytes.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    put_varint(&mut bytes, writer.strings.len() as u64);
    for string in &writer.strings {
        put_varint(&mut bytes, string.len() as u64);
        bytes.extend_from_slice(string.as_bytes());
    }
    bytes.extend_from_slice(&writer.payload);
    bytes
}

/// Decodes a syntax fragment written by [`encode_fragment`].
pub(crate) fn decode_fragment(bytes: &[u8]) -> Result<Syntax, WireError> {
    let mut reader = Reader {
        bytes,
        strings: Vec::new(),
    };
    let version = u32::from_le_bytes(reader.take(4)?.try_into().expect("four bytes"));
    if version != WIRE_VERSION {
        return Err(WireError::Version(version));
    }
    let count = reader.length()?;
    let mut strings = Vec::with_capacity(count.min(bytes.len()));
    for _ in 0..count {
        let length = reader.length()?;
        let text = std::str::from_utf8(reader.take(length)?).map_err(|_| WireError::Utf8)?;
        strings.push(Arc::<str>::from(text));
    }
    reader.strings = strings;
    let syntax = reader.fragment()?;
    if !reader.bytes.is_empty() {
        return Err(WireError::Trailing(reader.bytes.len()));
    }
    Ok(syntax)
}

fn split(bytes: &[u8], length: usize) -> Result<(&[u8], &[u8]), WireError> {
    if bytes.len() < length {
        return Err(WireError::Truncated);
    }
    Ok(bytes.split_at(length))
}

fn put_varint(bytes: &mut Vec<u8>, mut value: u64) {
    loop {
        let low = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            bytes.push(low);
            return;
        }
        bytes.push(low | 0x80);
    }
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[derive(Default)]
struct Writer {
    payload: Vec<u8>,
    strings: Vec<String>,
    interned: HashMap<String, u64>,
}

impl Writer {
    fn varint(&mut self, value: u64) {
        put_varint(&mut self.payload, value);
    }

    fn signed(&mut self, value: i64) {
        self.varint(zigzag(value));
    }

    fn usize(&mut self, value: usize) {
        self.varint(value as u64);
    }

    fn string(&mut self, text: &str) {
        let index = match self.interned.get(text) {
            Some(index) => *index,
            None => {
                let index = self.strings.len() as u64;
                self.strings.push(text.to_owned());
                self.interned.insert(text.to_owned(), index);
                index
            }
        };
        self.varint(index);
    }

    /// flags(user, source, location) [source] [start length] [line column]
    fn span(&mut self, span: &Span) {
        match span {
            Span::Compiler => self.payload.push(0),
            Span::User {
                source,
                range,
                location,
            } => {
                let mut flags = SPAN_USER;
                if source.is_some() {
                    flags |= SPAN_SOURCE;
                }
                if location.is_some() {
                    flags |= SPAN_LOCATION;
                }
                self.payload.push(flags);
                if let Some(source) = source {
                    self.string(source);
                }
                self.range(range, 0);
                if let Some(location) = location {
                    self.usize(location.line);
                    self.usize(location.column);
                }
            }
        }
    }

    /// The start relative to `base` and the length, both signed, so any
    /// range survives.
    fn range(&mut self, range: &Range<usize>, base: usize) {
        self.signed(range.start as i64 - base as i64);
        self.signed(range.end as i64 - range.start as i64);
    }

    /// flags(module, mark) span [module] [mark] origins(count (name span)*)
    /// offset [line column] tokens(count token*)
    ///
    /// token = kind:u8 flags(origin, hygiene):u8 text start length [origin]
    /// [module mark], with each token's start relative to the end of the
    /// token before it.
    fn fragment(&mut self, syntax: &Syntax) {
        let mut flags = 0;
        if syntax.definition_module.is_some() {
            flags |= FRAGMENT_MODULE;
        }
        if syntax.expansion_mark.is_some() {
            flags |= FRAGMENT_MARK;
        }
        self.payload.push(flags);
        self.span(&syntax.span);
        if let Some(module) = syntax.definition_module {
            self.usize(module);
        }
        if let Some(mark) = syntax.expansion_mark {
            self.varint(mark);
        }
        self.usize(syntax.identifier_origins.len());
        for (name, span) in &syntax.identifier_origins {
            self.string(name);
            self.span(span);
        }
        let tokens = syntax.tokens();
        // Where the slice starts in its stream: the offset, then the line and
        // column there, or 0 alone for a fragment that starts its stream.
        let offset = tokens.first().map_or(0, |token| token.span.start);
        self.usize(offset);
        if offset > 0 {
            let before = &syntax.token_stream()[..syntax.token_range.start];
            let (mut line, mut column) = (1usize, 1usize);
            for character in before.iter().flat_map(|token| token.text.chars()) {
                if character == '\n' {
                    line += 1;
                    column = 1;
                } else {
                    column += 1;
                }
            }
            self.usize(line);
            self.usize(column);
        }
        self.usize(tokens.len());
        let mut previous_end = 0;
        for token in tokens {
            let code = TOKEN_KINDS
                .iter()
                .position(|kind| *kind == token.kind)
                .expect("every token kind has a code");
            self.payload.push(code as u8);
            let mut flags = 0;
            if token.origin.is_some() {
                flags |= TOKEN_ORIGIN;
            }
            if token.hygiene.is_some() {
                flags |= TOKEN_HYGIENE;
            }
            self.payload.push(flags);
            self.string(&token.text);
            self.range(&token.span, previous_end);
            previous_end = token.span.end;
            if let Some(origin) = &token.origin {
                self.span(origin);
            }
            if let Some((module, mark)) = token.hygiene {
                self.usize(module);
                self.varint(mark);
            }
        }
    }

    /// kind, then a fragment for a node, a count and children for a list,
    /// nothing for an absent option, and a fragment and one child for a group.
    fn tree(&mut self, tree: &ArgTree) {
        match tree {
            ArgTree::Node(syntax) => {
                self.payload.push(NODE);
                self.fragment(syntax);
            }
            ArgTree::List(children) => {
                self.payload.push(LIST);
                self.usize(children.len());
                for child in children {
                    self.tree(child);
                }
            }
            ArgTree::Absent => self.payload.push(ABSENT),
            ArgTree::Group(source, contents) => {
                self.payload.push(GROUP);
                self.fragment(source);
                self.tree(contents);
            }
        }
    }

    /// definition_module next_mark call_site identifier_mark(0 or mark + 1)
    fn context(&mut self, context: &CallContext) {
        self.usize(context.definition_module.0);
        self.varint(context.next_mark);
        self.span(&context.call_site);
        self.varint(context.identifier_mark.map_or(0, |mark| mark + 1));
    }
}

/// Trivia standing in for the part of a stream before a slice: `offset` bytes
/// that end at `line` and `column`.
///
/// A token's span is an offset into the text of its whole stream, which the
/// parser rebuilds from the stream's tokens. A slice crosses without the rest
/// of its file, so the receiver puts this in front of it: spaces, then a
/// newline per line before the slice, then spaces up to its column. The
/// slice's spans keep their values, and positions in diagnostics keep their
/// line and column.
fn stream_prefix(offset: usize, line: usize, column: usize) -> Result<Vec<SyntaxToken>, WireError> {
    let newlines = line.checked_sub(1).ok_or(WireError::Invalid("line", 0))?;
    let indent = column.checked_sub(1).ok_or(WireError::Invalid("column", 0))?;
    let filler = offset
        .checked_sub(newlines)
        .and_then(|rest| rest.checked_sub(indent))
        .ok_or(WireError::Invalid("slice offset", offset as u64))?;
    let mut tokens = Vec::new();
    let mut start = 0;
    for (kind, text) in [
        (TokenKind::Whitespace, " ".repeat(filler)),
        (TokenKind::Newline, "\n".repeat(newlines)),
        (TokenKind::Whitespace, " ".repeat(indent)),
    ] {
        if text.is_empty() {
            continue;
        }
        let end = start + text.len();
        tokens.push(SyntaxToken { kind, text, span: start..end, origin: None, hygiene: None });
        start = end;
    }
    Ok(tokens)
}

struct Reader<'a> {
    bytes: &'a [u8],
    strings: Vec<Arc<str>>,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], WireError> {
        let (taken, rest) = split(self.bytes, length)?;
        self.bytes = rest;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn varint(&mut self) -> Result<u64, WireError> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            let low = u64::from(byte & 0x7f);
            if shift >= 64 || (shift == 63 && low > 1) {
                return Err(WireError::Overflow);
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn signed(&mut self) -> Result<i64, WireError> {
        Ok(unzigzag(self.varint()?))
    }

    fn usize(&mut self) -> Result<usize, WireError> {
        usize::try_from(self.varint()?).map_err(|_| WireError::Overflow)
    }

    /// A count, which can't be more than the bytes left, since every counted
    /// thing takes at least one byte.
    fn length(&mut self) -> Result<usize, WireError> {
        let length = self.usize()?;
        if length > self.bytes.len() {
            return Err(WireError::Truncated);
        }
        Ok(length)
    }

    fn string(&mut self) -> Result<Arc<str>, WireError> {
        let index = self.varint()?;
        usize::try_from(index)
            .ok()
            .and_then(|index| self.strings.get(index))
            .cloned()
            .ok_or(WireError::Invalid("string index", index))
    }

    fn flags(&mut self, what: &'static str, known: u8) -> Result<u8, WireError> {
        let flags = self.byte()?;
        if flags & !known != 0 {
            return Err(WireError::Invalid(what, u64::from(flags)));
        }
        Ok(flags)
    }

    fn span(&mut self) -> Result<Span, WireError> {
        let flags = self.flags("span flag set", SPAN_USER | SPAN_SOURCE | SPAN_LOCATION)?;
        if flags & SPAN_USER == 0 {
            if flags != 0 {
                return Err(WireError::Invalid("span flag set", u64::from(flags)));
            }
            return Ok(Span::Compiler);
        }
        let source = (flags & SPAN_SOURCE != 0)
            .then(|| self.string())
            .transpose()?;
        let range = self.range(0)?;
        let location = (flags & SPAN_LOCATION != 0)
            .then(|| -> Result<_, WireError> {
                Ok(SourceLocation {
                    line: self.usize()?,
                    column: self.usize()?,
                })
            })
            .transpose()?;
        Ok(Span::User {
            source,
            range,
            location,
        })
    }

    fn range(&mut self, base: usize) -> Result<Range<usize>, WireError> {
        let start = (base as i64)
            .checked_add(self.signed()?)
            .and_then(|start| usize::try_from(start).ok())
            .ok_or(WireError::Overflow)?;
        let end = (start as i64)
            .checked_add(self.signed()?)
            .and_then(|end| usize::try_from(end).ok())
            .ok_or(WireError::Overflow)?;
        Ok(start..end)
    }

    fn fragment(&mut self) -> Result<Syntax, WireError> {
        let flags = self.flags("fragment flag set", FRAGMENT_MODULE | FRAGMENT_MARK)?;
        let span = self.span()?;
        let definition_module = (flags & FRAGMENT_MODULE != 0)
            .then(|| self.usize())
            .transpose()?;
        let expansion_mark = (flags & FRAGMENT_MARK != 0)
            .then(|| self.varint())
            .transpose()?;
        let count = self.length()?;
        let mut identifier_origins = Vec::with_capacity(count);
        for _ in 0..count {
            let name = self.string()?.to_string();
            identifier_origins.push((name, self.span()?));
        }
        let offset = self.usize()?;
        let mut tokens = if offset > 0 {
            let (line, column) = (self.usize()?, self.usize()?);
            stream_prefix(offset, line, column)?
        } else {
            Vec::new()
        };
        let first = tokens.len();
        let count = self.length()?;
        tokens.reserve(count);
        let mut previous_end = 0;
        for _ in 0..count {
            let code = self.byte()?;
            let kind = *TOKEN_KINDS
                .get(usize::from(code))
                .ok_or(WireError::Invalid("token kind", u64::from(code)))?;
            let flags = self.flags("token flag set", TOKEN_ORIGIN | TOKEN_HYGIENE)?;
            let text = self.string()?.to_string();
            let span = self.range(previous_end)?;
            previous_end = span.end;
            let origin = (flags & TOKEN_ORIGIN != 0)
                .then(|| self.span())
                .transpose()?;
            let hygiene = (flags & TOKEN_HYGIENE != 0)
                .then(|| -> Result<_, WireError> { Ok((self.usize()?, self.varint()?)) })
                .transpose()?;
            tokens.push(SyntaxToken {
                kind,
                text,
                span,
                origin,
                hygiene,
            });
        }
        Ok(Syntax {
            pipe_stage: false,
            implied: false,
            // The receiver numbers the nodes it parses.
            id: SyntaxId(0),
            span,
            token_range: first..tokens.len(),
            tokens: tokens.into(),
            definition_module,
            expansion_mark,
            identifier_origins,
        })
    }

    fn tree(&mut self) -> Result<ArgTree, WireError> {
        match self.byte()? {
            NODE => Ok(ArgTree::Node(self.fragment()?)),
            LIST => {
                let count = self.length()?;
                let mut children = Vec::with_capacity(count);
                for _ in 0..count {
                    children.push(self.tree()?);
                }
                Ok(ArgTree::List(children))
            }
            ABSENT => Ok(ArgTree::Absent),
            GROUP => {
                let source = self.fragment()?;
                Ok(ArgTree::Group(source, Box::new(self.tree()?)))
            }
            kind => Err(WireError::Invalid("argument tree kind", u64::from(kind))),
        }
    }

    fn context(&mut self) -> Result<CallContext, WireError> {
        Ok(CallContext {
            definition_module: ModuleId(self.usize()?),
            next_mark: self.varint()?,
            call_site: self.span()?,
            identifier_mark: self.varint()?.checked_sub(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::macro_expand::syntax_ops::syntax_from_text;

    fn workspace_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the compiler has a workspace")
    }

    fn round_trip(message: &Message) -> Message {
        let bytes = encode(message);
        let (decoded, rest) = decode(&bytes).unwrap_or_else(|error| panic!("{error}"));
        assert!(rest.is_empty());
        decoded
    }

    /// Asserts that `decoded` has every field of `original` that crosses.
    fn assert_same_fragment(original: &Syntax, decoded: &Syntax, what: &str) {
        assert_eq!(original.span, decoded.span, "{what}: span");
        assert_eq!(
            original.definition_module, decoded.definition_module,
            "{what}: module"
        );
        assert_eq!(
            original.expansion_mark, decoded.expansion_mark,
            "{what}: mark"
        );
        assert_eq!(
            original.identifier_origins, decoded.identifier_origins,
            "{what}: origins"
        );
        assert_eq!(original.tokens(), decoded.tokens(), "{what}: tokens");
        assert_eq!(decoded.token_range.end, decoded.tokens.len(), "{what}: token range");
        // What stands in front of a slice is as long as what was in front of
        // it, so spans index the rebuilt stream's text.
        let prefix: usize = decoded.token_stream()[..decoded.token_range.start]
            .iter()
            .map(|token| token.text.len())
            .sum();
        assert_eq!(
            prefix,
            original.tokens().first().map_or(0, |token| token.span.start),
            "{what}: prefix"
        );
    }

    #[test]
    fn a_slice_parses_and_reports_where_it_was() {
        use crate::macro_expand::syntax_ops::{describe_position, syntax_slice};

        let mut next = 0;
        let file = syntax_from_text("def a = 1\n\n// é\ndef b = (x + 2) * 3\n", &mut next);
        let start = file.tokens().iter().position(|token| token.text == "(").unwrap();
        let end = file.tokens().iter().rposition(|token| token.text == "3").unwrap() + 1;
        let slice = syntax_slice(&file, start, end);
        let Message::Result { syntax: decoded, .. } =
            round_trip(&Message::Result { syntax: slice.clone(), next_mark: 1 })
        else {
            panic!("a result came back as something else");
        };
        assert_same_fragment(&slice, &decoded, "slice");
        assert_eq!(describe_position(&decoded, 0), describe_position(&slice, 0));
        assert_eq!(describe_position(&decoded, 0), "line 4, column 9");
        let parsed = staple_syntax::parse_expression_fragment(&decoded, &mut next)
            .expect("the slice parses");
        assert_eq!(parsed.syntax().text(), "(x + 2) * 3");
        let original = staple_syntax::parse_expression_fragment(&slice, &mut next)
            .expect("the original slice parses");
        assert_eq!(parsed.syntax().span, original.syntax().span);
    }

    fn assert_same_tree(original: &ArgTree, decoded: &ArgTree, what: &str) {
        match (original, decoded) {
            (ArgTree::Node(left), ArgTree::Node(right)) => assert_same_fragment(left, right, what),
            (ArgTree::List(left), ArgTree::List(right)) => {
                assert_eq!(left.len(), right.len(), "{what}: list length");
                for (left, right) in left.iter().zip(right) {
                    assert_same_tree(left, right, what);
                }
            }
            (ArgTree::Absent, ArgTree::Absent) => {}
            (ArgTree::Group(left, left_contents), ArgTree::Group(right, right_contents)) => {
                assert_same_fragment(left, right, what);
                assert_same_tree(left_contents, right_contents, what);
            }
            (left, right) => panic!("{what}: {left:?} came back as {right:?}"),
        }
    }

    fn assert_same_result(syntax: &Syntax, next_mark: u64, what: &str) {
        let message = Message::Result {
            syntax: syntax.clone(),
            next_mark,
        };
        match round_trip(&message) {
            Message::Result {
                syntax: decoded,
                next_mark: decoded_mark,
            } => {
                assert_same_fragment(syntax, &decoded, what);
                assert_eq!(decoded_mark, next_mark, "{what}: next mark");
            }
            other => panic!("{what}: a result came back as {other:?}"),
        }
    }

    /// The inputs the expansion comparison reads: the standard library, the
    /// examples, the fixtures and the code generation corpus.
    fn corpus_inputs() -> Vec<(String, String)> {
        fn collect(directory: &Path, inputs: &mut Vec<(String, String)>) {
            let mut entries = std::fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("`{}` should list: {error}", directory.display()))
                .map(|entry| entry.expect("a directory entry").path())
                .collect::<Vec<PathBuf>>();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    collect(&path, inputs);
                } else if path.extension().is_some_and(|extension| extension == "sta") {
                    let source = std::fs::read_to_string(&path).expect("a corpus file reads");
                    inputs.push((path.display().to_string(), source));
                }
            }
        }
        let root = workspace_root();
        let mut inputs = Vec::new();
        for directory in [
            "stdlib/std",
            "staple-compiler/examples",
            "staple-compiler/tests/fixtures",
        ] {
            collect(&root.join(directory), &mut inputs);
        }
        for program in crate::codegen::corpus::codegen_corpus() {
            let source = match program.source {
                crate::codegen::corpus::CorpusSource::Inline(source) => source.to_owned(),
                crate::codegen::corpus::CorpusSource::File(path) => {
                    std::fs::read_to_string(root.join(path)).expect("a corpus file reads")
                }
            };
            inputs.push((program.name.to_owned(), source));
        }
        inputs
    }

    /// Gives every other token an origin and every third one hygiene, so
    /// every field of a token crosses.
    fn with_every_field(syntax: &Syntax, name: &str) -> Syntax {
        let source: Arc<str> = Arc::from(name);
        let tokens = syntax
            .tokens()
            .iter()
            .enumerate()
            .map(|(index, token)| {
                let mut token = token.clone();
                if index % 2 == 0 {
                    token.origin = Some(Span::User {
                        source: Some(source.clone()),
                        range: token.span.clone(),
                        location: Some(SourceLocation {
                            line: index + 1,
                            column: index % 80,
                        }),
                    });
                }
                if index % 3 == 0 {
                    token.hygiene = Some((index % 7, index as u64 * 1_000_003));
                }
                token
            })
            .collect::<Vec<_>>();
        let mut syntax = syntax.clone();
        syntax.token_range = 0..tokens.len();
        syntax.tokens = tokens.into();
        syntax.span = Span::User {
            source: Some(source),
            range: syntax.span.to_range(),
            location: Some(SourceLocation { line: 1, column: 1 }),
        };
        syntax.definition_module = Some(3);
        syntax.expansion_mark = Some(u64::MAX);
        syntax.identifier_origins = vec![("name".to_owned(), Span::Compiler)];
        syntax
    }

    #[test]
    fn every_corpus_token_round_trips() {
        let inputs = corpus_inputs();
        assert!(inputs.len() > 200, "the corpus has {} inputs", inputs.len());
        let mut next = 0;
        let mut tokens = 0;
        for (name, source) in &inputs {
            let syntax = syntax_from_text(source, &mut next);
            assert_eq!(syntax.text(), *source, "{name} lexes losslessly");
            tokens += syntax.tokens().len();
            assert_same_result(&syntax, 7, name);
            assert_same_result(&with_every_field(&syntax, name), 1 << 40, name);
        }
        assert!(tokens > 100_000, "the corpus has {tokens} tokens");
        eprintln!("{} inputs, {tokens} tokens", inputs.len());
    }

    #[test]
    fn calls_and_errors_round_trip() {
        let mut next = 0;
        let fragment = |text: &str, next: &mut usize| syntax_from_text(text, next);
        let arguments = vec![
            ArgTree::Node(fragment("f x", &mut next)),
            ArgTree::List(vec![
                ArgTree::Absent,
                ArgTree::Node(fragment("1", &mut next)),
            ]),
            ArgTree::Group(
                with_every_field(&fragment("(a, b)", &mut next), "group.sta"),
                Box::new(ArgTree::List(Vec::new())),
            ),
            ArgTree::Absent,
        ];
        for identifier_mark in [None, Some(0), Some(41)] {
            let context = CallContext {
                definition_module: ModuleId(12),
                next_mark: 300,
                call_site: Span::User {
                    source: Some(Arc::from("call.sta")),
                    range: 10..20,
                    location: Some(SourceLocation { line: 2, column: 5 }),
                },
                identifier_mark,
            };
            let message = Message::Call {
                entry: "operator.656e747279".to_owned(),
                context: context.clone(),
                arguments: arguments.clone(),
            };
            let Message::Call {
                entry,
                context: decoded,
                arguments: decoded_arguments,
            } = round_trip(&message)
            else {
                panic!("a call came back as something else");
            };
            assert_eq!(entry, "operator.656e747279");
            assert_eq!(decoded.definition_module, context.definition_module);
            assert_eq!(decoded.next_mark, context.next_mark);
            assert_eq!(decoded.call_site, context.call_site);
            assert_eq!(decoded.identifier_mark, context.identifier_mark);
            assert_same_tree(
                &ArgTree::List(arguments.clone()),
                &ArgTree::List(decoded_arguments),
                "call",
            );
        }
        let message = Message::Error {
            message: "expected an expression, found `)`".to_owned(),
        };
        let Message::Error { message } = round_trip(&message) else {
            panic!("an error came back as something else");
        };
        assert_eq!(message, "expected an expression, found `)`");
    }

    #[test]
    fn token_kinds_have_distinct_codes() {
        assert!(TOKEN_KINDS.len() <= usize::from(u8::MAX));
        for (index, kind) in TOKEN_KINDS.iter().enumerate() {
            assert_eq!(
                TOKEN_KINDS.iter().position(|other| other == kind),
                Some(index),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn frames_follow_one_another() {
        let first = encode(&Message::Error {
            message: "one".to_owned(),
        });
        let second = encode(&Message::Error {
            message: "two".to_owned(),
        });
        let stream = [first.as_slice(), second.as_slice()].concat();
        let (message, rest) = decode(&stream).expect("the first frame decodes");
        assert!(matches!(message, Message::Error { message } if message == "one"));
        let (message, rest) = decode(rest).expect("the second frame decodes");
        assert!(matches!(message, Message::Error { message } if message == "two"));
        assert!(rest.is_empty());
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let mut next = 0;
        let frame = encode(&Message::Result {
            syntax: syntax_from_text("a b", &mut next),
            next_mark: 1,
        });
        for end in 0..frame.len() {
            assert_eq!(
                decode(&frame[..end]).err(),
                Some(WireError::Truncated),
                "cut at {end}"
            );
        }

        let mut other_version = frame.clone();
        other_version[4..8].copy_from_slice(&(WIRE_VERSION + 1).to_le_bytes());
        assert_eq!(
            decode(&other_version).err(),
            Some(WireError::Version(WIRE_VERSION + 1))
        );

        let mut bad_tag = frame.clone();
        bad_tag[8] = 9;
        assert_eq!(
            decode(&bad_tag).err(),
            Some(WireError::Invalid("message tag", 9))
        );

        // A body that claims one more byte than its payload uses.
        let mut padded = frame.clone();
        padded.push(0);
        let length = u32::from_le_bytes(padded[..4].try_into().unwrap()) + 1;
        padded[..4].copy_from_slice(&length.to_le_bytes());
        assert_eq!(decode(&padded).err(), Some(WireError::Trailing(1)));
    }

    /// Records the `@derive_eq` calls of 20 types of 50 fields, and the size
    /// of each call and result on the wire.
    #[test]
    fn derive_eq_calls_round_trip_with_their_sizes() {
        use crate::ProgramLoader;
        use crate::macro_expand::compiled_call::record_calls;

        let fields = (0..50)
            .map(|index| format!("f{index}: I32"))
            .collect::<Vec<_>>()
            .join(", ");
        let types = (0..20)
            .map(|index| format!("@derive_eq\ntype P{index} wrap ({fields})\n"))
            .collect::<String>();
        let source = format!("use std.cmp.(derive_eq)\n{types}");
        let root = workspace_root();
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, root)
            .expect("the input loads");
        // Expansion needs a large stack, and records on the thread it runs on.
        // Recorded values hold the interpreter's own values, which stay on
        // that thread, so each call leaves it as a message and its result's
        // fragments.
        let calls = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let (expanded, calls) = record_calls(|| {
                    crate::macro_expand::expand_program_checked(program, false, None, false)
                });
                let (_, _, diagnostics) = expanded
                    .unwrap_or_else(|diagnostics| panic!("the input expands: {diagnostics:?}"));
                assert!(diagnostics.is_empty(), "the input expands: {diagnostics:?}");
                calls
                    .into_iter()
                    .filter(|call| call.declaration.name == "derive_eq")
                    .enumerate()
                    .map(|(index, call)| {
                        let message = Message::Call {
                            entry: crate::macro_expand::check_program::entry_function_name(
                                &call.declaration,
                            ),
                            context: CallContext {
                                definition_module: call.definition_module,
                                next_mark: 1_000 + index as u64,
                                call_site: call.call_site.clone(),
                                identifier_mark: None,
                            },
                            arguments: call.arguments.clone(),
                        };
                        let result = call.interpreted.expect("the interpreter expanded the call");
                        (message, result.syntaxes())
                    })
                    .collect::<Vec<_>>()
            })
            .expect("the expansion thread spawns")
            .join()
            .expect("expansion doesn't panic");
        assert_eq!(calls.len(), 20);
        let mut call_sizes = Vec::new();
        let mut result_sizes = Vec::new();
        for (message, results) in calls {
            call_sizes.push(encode(&message).len());
            let Message::Call {
                arguments: original,
                ..
            } = &message
            else {
                unreachable!("only calls are recorded");
            };
            let Message::Call { arguments, .. } = round_trip(&message) else {
                panic!("a call came back as something else");
            };
            assert_same_tree(
                &ArgTree::List(original.clone()),
                &ArgTree::List(arguments),
                "derive_eq",
            );
            let mut size = 0;
            for syntax in results {
                assert_same_result(&syntax, 2_000, "derive_eq result");
                size += encode(&Message::Result {
                    syntax,
                    next_mark: 2_000,
                })
                .len();
            }
            result_sizes.push(size);
        }
        let summary = |sizes: &[usize]| {
            format!(
                "{} to {} bytes, {} in all",
                sizes.iter().min().unwrap(),
                sizes.iter().max().unwrap(),
                sizes.iter().sum::<usize>()
            )
        };
        eprintln!("derive_eq calls: {}", summary(&call_sizes));
        eprintln!("derive_eq results: {}", summary(&result_sizes));
    }
}
