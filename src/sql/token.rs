//! The SQL tokenizer.
//!
//! Splits SQL text into [`Token`]s following SQLite's lexical rules
//! (`tokenize.c`): case-insensitive keywords, `'…'` string literals with `''`
//! escaping, `x'…'` blob literals, `"…"`/`[…]`/`` `…` `` quoted identifiers,
//! `--` line and `/* */` block comments, numeric literals (including `0x` hex
//! and floats), and `?`, `?N`, `:name`, `@name`, `$name` parameters.
//!
//! Keyword-vs-identifier is *not* decided here: bare words become
//! [`Token::Word`], and the parser decides whether a given word acts as a
//! keyword in context (matching SQLite, where many keywords are also usable as
//! identifiers). Quoted identifiers become [`Token::Ident`] and are never
//! keywords.

use crate::error::{Error, Result};
use alloc::string::String;
use alloc::vec::Vec;

/// A lexical token.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A bare word: a keyword *or* an identifier, decided by the parser.
    Word(String),
    /// A quoted identifier (`"x"`, `[x]`, `` `x` ``); never a keyword.
    Ident(String),
    /// An integer literal.
    Integer(i64),
    /// The decimal integer literal `9223372036854775808` (2^63) — the one
    /// magnitude that overflows `i64` as a positive value but whose negation is
    /// exactly `i64::MIN`. Used positively it is a real (like any overflowing
    /// integer); negated, the parser folds it to `Integer(i64::MIN)`, matching
    /// SQLite's handling of the `-9223372036854775808` literal.
    Int2Pow63,
    /// A floating-point literal.
    Float(f64),
    /// A string literal (already unescaped).
    Str(String),
    /// A blob literal from `x'…'`.
    Blob(Vec<u8>),
    /// A bound parameter (`?`, `?12`, `:name`, `@name`, `$name`).
    Param(Param),

    // Punctuation & operators.
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `.`
    Dot,
    /// `*`
    Star,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `=` or `==`
    Eq,
    /// `!=` or `<>`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `||`
    Concat,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `~`
    BitNot,
    /// `<<`
    LShift,
    /// `>>`
    RShift,
    /// `->` — JSON extract (as JSON).
    Arrow,
    /// `->>` — JSON extract (as text).
    Arrow2,
}

/// A bound parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    /// An anonymous `?`.
    Anonymous,
    /// A numbered `?N`.
    Numbered(u32),
    /// A named `:name`, `@name`, or `$name` (the sigil is preserved).
    Named(String),
}

/// A token together with its byte span in the source, for diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    /// The token.
    pub token: Token,
    /// Byte offset of the token's first character.
    pub start: usize,
    /// Byte offset just past the token's last character.
    pub end: usize,
}

/// Tokenize `sql` into a vector of spanned tokens (no trailing EOF marker).
pub fn tokenize(sql: &str) -> Result<Vec<Spanned>> {
    Tokenizer::new(sql).run()
}

struct Tokenizer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Byte offset where the token currently being lexed began. Used to render
    /// a lexing failure as SQLite's `unrecognized token: "<source slice>"`.
    tok_start: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Tokenizer<'a> {
        Tokenizer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            tok_start: 0,
        }
    }

    fn run(mut self) -> Result<Vec<Spanned>> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia()?;
            let start = self.pos;
            self.tok_start = start;
            let Some(c) = self.peek() else { break };
            let token = self.next_token(c)?;
            out.push(Spanned {
                token,
                start,
                end: self.pos,
            });
        }
        Ok(out)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.pos + ahead).copied()
    }

    fn err(&self, msg: &str) -> Error {
        Error::Parse(alloc::format!("{msg} at byte {}", self.pos))
    }

    /// SQLite reports every lexing failure as `unrecognized token: "X"`, where
    /// `X` is the verbatim source text from the current token's start to where
    /// the lexer gave up (a stray `^`, a whole unterminated `'abc`, a malformed
    /// `x'zz'`, a number run with a bad suffix like `123abc`). The caller is
    /// responsible for advancing `self.pos` to the end of that run first.
    fn unrecognized(&self) -> Error {
        Error::Parse(alloc::format!(
            "unrecognized token: \"{}\"",
            &self.src[self.tok_start..self.pos]
        ))
    }

    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.pos += 1;
                }
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    // Line comment to end of line.
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == b'\n' {
                            break;
                        }
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    self.pos += 2;
                    loop {
                        match self.peek() {
                            // An unterminated block comment runs off the end of
                            // the input — SQLite reports this as `incomplete
                            // input`, like any other premature end.
                            None => return Err(Error::Parse("incomplete input".into())),
                            Some(b'*') if self.peek_at(1) == Some(b'/') => {
                                self.pos += 2;
                                break;
                            }
                            Some(_) => self.pos += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn next_token(&mut self, c: u8) -> Result<Token> {
        match c {
            b'(' => self.single(Token::LParen),
            b')' => self.single(Token::RParen),
            b',' => self.single(Token::Comma),
            b';' => self.single(Token::Semicolon),
            b'+' => self.single(Token::Plus),
            b'-' => {
                self.pos += 1; // consume '-'
                if self.peek() == Some(b'>') {
                    self.pos += 1; // consume '>'
                    if self.peek() == Some(b'>') {
                        self.single(Token::Arrow2) // ->>
                    } else {
                        Ok(Token::Arrow) // ->
                    }
                } else {
                    Ok(Token::Minus)
                }
            }
            b'%' => self.single(Token::Percent),
            b'~' => self.single(Token::BitNot),
            b'*' => self.single(Token::Star),
            b'/' => self.single(Token::Slash),
            b'=' => {
                self.pos += 1;
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                }
                Ok(Token::Eq)
            }
            b'<' => {
                self.pos += 1;
                match self.peek() {
                    Some(b'=') => self.single(Token::LtEq),
                    Some(b'>') => self.single(Token::NotEq),
                    Some(b'<') => self.single(Token::LShift),
                    _ => Ok(Token::Lt),
                }
            }
            b'>' => {
                self.pos += 1;
                match self.peek() {
                    Some(b'=') => self.single(Token::GtEq),
                    Some(b'>') => self.single(Token::RShift),
                    _ => Ok(Token::Gt),
                }
            }
            b'!' => {
                self.pos += 1;
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Ok(Token::NotEq)
                } else {
                    Err(self.err("unexpected '!' (did you mean '!=' ?)"))
                }
            }
            b'|' => {
                self.pos += 1;
                if self.peek() == Some(b'|') {
                    self.single(Token::Concat)
                } else {
                    Ok(Token::BitOr)
                }
            }
            b'&' => self.single(Token::BitAnd),
            b'.' => {
                // A leading-dot float like `.5`?
                if matches!(self.peek_at(1), Some(d) if d.is_ascii_digit()) {
                    self.number()
                } else {
                    self.single(Token::Dot)
                }
            }
            b'\'' => self.string_literal(),
            b'"' => self.quoted_ident(b'"'),
            b'[' => self.bracket_ident(),
            b'`' => self.quoted_ident(b'`'),
            b'?' | b':' | b'@' | b'$' => self.parameter(c),
            b'x' | b'X' if self.peek_at(1) == Some(b'\'') => self.blob_literal(),
            d if d.is_ascii_digit() => self.number(),
            w if is_ident_start(w) => Ok(self.word()),
            _ => {
                // An unhandled byte here is always single-byte ASCII (every
                // byte >= 0x80 starts an identifier). Consume it so the reported
                // token is the character itself.
                self.pos += 1;
                Err(self.unrecognized())
            }
        }
    }

    fn single(&mut self, t: Token) -> Result<Token> {
        self.pos += 1;
        Ok(t)
    }

    fn word(&mut self) -> Token {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.pos += 1;
        }
        Token::Word(String::from(&self.src[start..self.pos]))
    }

    fn quoted_ident(&mut self, quote: u8) -> Result<Token> {
        self.pos += 1; // opening quote
                       // Accumulate by slicing the (UTF-8) source between escapes; the only
                       // split points are the ASCII quote bytes, which are never inside a
                       // multi-byte code point, so every slice is a valid `&str`.
        let mut s = String::new();
        let mut seg = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.unrecognized()),
                Some(c) if c == quote => {
                    s.push_str(&self.src[seg..self.pos]);
                    self.pos += 1;
                    if self.peek() == Some(quote) {
                        s.push(quote as char); // doubled quote = escaped quote
                        self.pos += 1;
                        seg = self.pos;
                    } else {
                        return Ok(Token::Ident(s));
                    }
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn bracket_ident(&mut self) -> Result<Token> {
        self.pos += 1; // '['
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b']' {
                let s = String::from(&self.src[start..self.pos]);
                self.pos += 1;
                return Ok(Token::Ident(s));
            }
            self.pos += 1;
        }
        Err(self.err("unterminated [identifier]"))
    }

    fn string_literal(&mut self) -> Result<Token> {
        self.pos += 1; // opening quote
        let mut s = String::new();
        let mut seg = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.unrecognized()),
                Some(b'\'') => {
                    s.push_str(&self.src[seg..self.pos]);
                    self.pos += 1;
                    if self.peek() == Some(b'\'') {
                        s.push('\''); // doubled quote = escaped quote
                        self.pos += 1;
                        seg = self.pos;
                    } else {
                        return Ok(Token::Str(s));
                    }
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn blob_literal(&mut self) -> Result<Token> {
        self.pos += 2; // x'
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c != b'\'') {
            self.pos += 1;
        }
        if self.peek() != Some(b'\'') {
            return Err(self.unrecognized());
        }
        let hex = &self.src[start..self.pos];
        self.pos += 1; // closing quote
        if !hex.len().is_multiple_of(2) {
            return Err(self.unrecognized());
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        let hb = hex.as_bytes();
        let mut i = 0;
        while i < hb.len() {
            let hi = hex_val(hb[i]).ok_or_else(|| self.unrecognized())?;
            let lo = hex_val(hb[i + 1]).ok_or_else(|| self.unrecognized())?;
            bytes.push((hi << 4) | lo);
            i += 2;
        }
        Ok(Token::Blob(bytes))
    }

    fn number(&mut self) -> Result<Token> {
        let start = self.pos;
        // Hex integer.
        if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x') | Some(b'X')) {
            self.pos += 2;
            let hstart = self.pos;
            self.consume_digit_run(|c| c.is_ascii_hexdigit());
            self.reject_number_suffix()?;
            // SQLite allows `_` digit separators between digits (3.46+); strip
            // them before parsing.
            let digits = self.src[hstart..self.pos].replace('_', "");
            // `0x` with no digits is just an unrecognized token. A non-empty
            // run that overflows 64 bits is a *recognized* hex literal that
            // SQLite then rejects with a dedicated message
            // (`hex literal too big: 0x…`), not a generic unrecognized token.
            if digits.is_empty() {
                return Err(self.unrecognized());
            }
            let v = u64::from_str_radix(&digits, 16).map_err(|_| {
                Error::Parse(alloc::format!(
                    "hex literal too big: {}",
                    &self.src[start..self.pos]
                ))
            })?;
            return Ok(Token::Integer(v as i64));
        }

        let mut is_float = false;
        self.consume_digit_run(|c| c.is_ascii_digit());
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            self.consume_digit_run(|c| c.is_ascii_digit());
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            self.consume_digit_run(|c| c.is_ascii_digit());
        }
        self.reject_number_suffix()?;
        let text = self.src[start..self.pos].replace('_', "");
        if is_float {
            text.parse::<f64>()
                .map(Token::Float)
                .map_err(|_| self.unrecognized())
        } else {
            match text.parse::<i64>() {
                Ok(i) => Ok(Token::Integer(i)),
                // `2^63` is special: positive it is a real, but `-2^63` is exactly
                // i64::MIN, so the parser may fold a leading minus into an integer.
                Err(_) if text == "9223372036854775808" => Ok(Token::Int2Pow63),
                // Other integers that overflow i64 become floats, as in SQLite.
                Err(_) => text
                    .parse::<f64>()
                    .map(Token::Float)
                    .map_err(|_| self.unrecognized()),
            }
        }
    }

    /// Reject a numeric literal that is immediately followed by an identifier
    /// character (`123abc`, `0x1p4`, `12e3f`): SQLite treats the whole run as one
    /// "unrecognized token" rather than a number adjacent to a name.
    fn reject_number_suffix(&mut self) -> Result<()> {
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                // Consume the whole adjacent identifier run so the reported token
                // is the entire offending span (`123abc`, not `123`), matching
                // SQLite's `unrecognized token: "123abc"`.
                while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
                    self.pos += 1;
                }
                Err(self.unrecognized())
            }
            _ => Ok(()),
        }
    }

    /// Advance over a run of digits accepted by `is_digit`, allowing single `_`
    /// separators that sit between two such digits (SQLite 3.46+).
    fn consume_digit_run(&mut self, is_digit: impl Fn(u8) -> bool) {
        while let Some(c) = self.peek() {
            // A digit, or a `_` separator sitting between two digits.
            let sep_ok = c == b'_'
                && matches!(self.peek_at(1), Some(n) if is_digit(n))
                && self.pos > 0
                && self
                    .src
                    .as_bytes()
                    .get(self.pos - 1)
                    .is_some_and(|&p| is_digit(p));
            if is_digit(c) || sep_ok {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parameter(&mut self, sigil: u8) -> Result<Token> {
        self.pos += 1; // sigil
        match sigil {
            b'?' => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
                if self.pos == start {
                    Ok(Token::Param(Param::Anonymous))
                } else {
                    let n = self.src[start..self.pos]
                        .parse::<u32>()
                        .map_err(|_| self.err("invalid parameter number"))?;
                    Ok(Token::Param(Param::Numbered(n)))
                }
            }
            _ => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(self.unrecognized());
                }
                let mut name = String::new();
                name.push(sigil as char);
                name.push_str(&self.src[start..self.pos]);
                Ok(Token::Param(Param::Named(name)))
            }
        }
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .map(|s| s.token)
            .collect()
    }

    #[test]
    fn number_followed_by_identifier_is_rejected() {
        // A numeric literal immediately followed by an identifier character is one
        // "unrecognized token", not a number adjacent to a name — as in sqlite.
        for bad in [
            "123abc", "1.5xyz", "0xffz", "0x1p4", "12e3f", "5abc", "1e3e4", "0b1",
        ] {
            assert!(
                tokenize(&alloc::format!("SELECT {bad}")).is_err(),
                "expected {bad} to be rejected"
            );
        }
        // Valid numbers — and a number properly separated from a name — still lex.
        for ok in [
            "123", "1.5", "0xff", "1e3", ".5", "5.", "0x0", "5e+3", "1_000", "0xff_ff", "5 abc",
            "5 AS abc",
        ] {
            assert!(
                tokenize(&alloc::format!("SELECT {ok}")).is_ok(),
                "expected {ok} to lex"
            );
        }
    }

    #[test]
    fn keywords_and_identifiers_are_words() {
        assert_eq!(
            toks("SELECT a FROM t"),
            vec![
                Token::Word("SELECT".into()),
                Token::Word("a".into()),
                Token::Word("FROM".into()),
                Token::Word("t".into()),
            ]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            toks("a >= 1 AND b <> 2 OR c || d"),
            vec![
                Token::Word("a".into()),
                Token::GtEq,
                Token::Integer(1),
                Token::Word("AND".into()),
                Token::Word("b".into()),
                Token::NotEq,
                Token::Integer(2),
                Token::Word("OR".into()),
                Token::Word("c".into()),
                Token::Concat,
                Token::Word("d".into()),
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(toks("42"), vec![Token::Integer(42)]);
        assert_eq!(toks("2.75"), vec![Token::Float(2.75)]);
        assert_eq!(toks(".5"), vec![Token::Float(0.5)]);
        assert_eq!(toks("1e3"), vec![Token::Float(1000.0)]);
        assert_eq!(toks("0xff"), vec![Token::Integer(255)]);
    }

    #[test]
    fn strings_and_blobs() {
        assert_eq!(toks("'hi'"), vec![Token::Str("hi".into())]);
        assert_eq!(toks("'it''s'"), vec![Token::Str("it's".into())]);
        assert_eq!(toks("x'01ff'"), vec![Token::Blob(vec![1, 255])]);
    }

    #[test]
    fn quoted_identifiers() {
        assert_eq!(toks("\"select\""), vec![Token::Ident("select".into())]);
        assert_eq!(toks("[a b]"), vec![Token::Ident("a b".into())]);
        assert_eq!(toks("`x`"), vec![Token::Ident("x".into())]);
        assert_eq!(toks("\"a\"\"b\""), vec![Token::Ident("a\"b".into())]);
    }

    #[test]
    fn parameters() {
        assert_eq!(toks("?"), vec![Token::Param(Param::Anonymous)]);
        assert_eq!(toks("?12"), vec![Token::Param(Param::Numbered(12))]);
        assert_eq!(
            toks(":name"),
            vec![Token::Param(Param::Named(":name".into()))]
        );
        assert_eq!(toks("$x"), vec![Token::Param(Param::Named("$x".into()))]);
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            toks("SELECT -- a comment\n 1 /* block */ + 2"),
            vec![
                Token::Word("SELECT".into()),
                Token::Integer(1),
                Token::Plus,
                Token::Integer(2),
            ]
        );
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(tokenize("'oops").is_err());
        assert!(tokenize("/* nope").is_err());
    }

    #[test]
    fn utf8_identifier_preserved() {
        // A non-ASCII identifier should tokenize as a single word with its bytes
        // intact (bare words slice the source directly).
        assert_eq!(toks("café"), vec![Token::Word("café".into())]);
    }
}
