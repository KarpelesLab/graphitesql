//! The SQL front end: tokenizer, AST, and parser.
//!
//! Text enters as a `&str` and leaves as [`ast::Statement`]s. The pipeline is
//! [`token`] → [`parser`] → [`ast`]. The grammar tracks SQLite's `parse.y`; the
//! parser is hand-written recursive descent with a Pratt expression core so we
//! get precise control over error messages and operator precedence.

pub mod ast;
pub mod parser;
pub mod token;

pub use parser::{parse, parse_one};
