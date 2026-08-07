//! A small, lossless parser for staple.
//!
//! The lexer deliberately emits trivia tokens. Every AST node keeps the exact
//! token slice from which it was parsed, so [`SyntaxNode::text`] is a lossless
//! reconstruction of its source.

mod ast;
mod combinator;
mod lexer;
mod parser;

pub use ast::*;
pub use parser::parse;
