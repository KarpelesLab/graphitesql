//! The SQL tokenizer.
//!
//! Splits SQL text into [`Token`]s following SQLite's lexical rules
//! (`tokenize.c`): case-insensitive keywords, `'…'` string literals with `''`
//! escaping, `x'…'` blob literals, `"…"`/`[…]`/`` `…` `` quoted identifiers,
//! `--` line and `/* */` block comments, numeric literals (including `0x` hex
//! and floats), and `?`, `?N`, `:name`, `@name`, `$name`, `#name` parameters.
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
    /// A bound parameter (`?`, `?12`, `:name`, `@name`, `$name`, `#name`).
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
    /// A named `:name`, `@name`, `$name`, or `#name` (the sigil is preserved).
    /// A `#`-sigil name whose first character is a digit is SQLite's internal
    /// register reference, valid only in a nested parse — the parser rejects it
    /// in user SQL with `near "#N": syntax error`.
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
            self.skip_trivia();
            let start = self.pos;
            self.tok_start = start;
            let Some(c) = self.peek() else { break };
            // SQLite's SQL strings are C strings: `sqlite3RunParser` treats a
            // NUL byte as the end of the input (tokenize.c CC_NUL), silently
            // ignoring anything after it.
            if c == 0 {
                break;
            }
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

    /// SQLite reports every lexing failure as `unrecognized token: "X"`, where
    /// `X` is the verbatim source text from the current token's start to where
    /// the lexer gave up (a stray `^`, a whole unterminated `'abc`, a malformed
    /// `x'zz'`, a number run with a bad suffix like `123abc`). The caller is
    /// responsible for advancing `self.pos` to the end of that run first.
    /// The error carries the token's byte offset (sqlite's `%T` formatting of
    /// the message records it for `sqlite3_error_offset`, driving the CLI
    /// caret).
    fn unrecognized(&self) -> Error {
        Error::ParseAt(
            alloc::format!(
                "unrecognized token: \"{}\"",
                &self.src[self.tok_start..self.pos]
            ),
            self.tok_start,
        )
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.pos += 1;
                }
                // A UTF-8 BOM (`EF BB BF`) at a token boundary — tokenize.c
                // classes 0xEF as CC_BOM and `sqlite3GetToken` returns TK_SPACE
                // for the full 3-byte sequence, so it is skipped like
                // whitespace. A 0xEF that does not start a BOM is an
                // identifier byte, handled by the normal path.
                Some(0xEF) if self.peek_at(1) == Some(0xBB) && self.peek_at(2) == Some(0xBF) => {
                    self.pos += 3;
                }
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    // Line comment to end of line (or NUL, which ends the SQL).
                    while let Some(c) = self.peek() {
                        if c == 0 {
                            break;
                        }
                        self.pos += 1;
                        if c == b'\n' {
                            break;
                        }
                    }
                }
                // tokenize.c CC_SLASH: `if( z[1]!='*' || z[2]==0 )` — a `/*`
                // with nothing after the `*` is NOT a comment; the `/` lexes as
                // a division operator and the `*` as a separate star.
                Some(b'/')
                    if self.peek_at(1) == Some(b'*')
                        && !matches!(self.peek_at(2), None | Some(0)) =>
                {
                    self.pos += 2;
                    loop {
                        match self.peek() {
                            // An unterminated block comment that runs to the end
                            // of the input is still TK_COMMENT in SQLite (the
                            // CC_SLASH scan just stops at NUL) — i.e. trailing
                            // whitespace, not an error.
                            None | Some(0) => break,
                            Some(b'*') if self.peek_at(1) == Some(b'/') => {
                                self.pos += 2;
                                break;
                            }
                            Some(_) => self.pos += 1,
                        }
                    }
                }
                _ => return,
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
                    // tokenize.c CC_BANG: a lone `!` is TK_ILLEGAL —
                    // `unrecognized token: "!"`, not a bespoke hint.
                    Err(self.unrecognized())
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
            b'?' | b':' | b'@' | b'$' | b'#' => self.parameter(c),
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
                // tokenize.c CC_QUOTE scans to the closing quote or NUL/end of
                // input; unterminated is TK_ILLEGAL spanning the whole run.
                None | Some(0) => return Err(self.unrecognized()),
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
            if c == 0 {
                break;
            }
            if c == b']' {
                let s = String::from(&self.src[start..self.pos]);
                self.pos += 1;
                return Ok(Token::Ident(s));
            }
            self.pos += 1;
        }
        // tokenize.c CC_QUOTE2: an unterminated `[…` scans to the end of the
        // input and is TK_ILLEGAL — reported like any other bad token.
        Err(self.unrecognized())
    }

    fn string_literal(&mut self) -> Result<Token> {
        self.pos += 1; // opening quote
        let mut s = String::new();
        let mut seg = self.pos;
        loop {
            match self.peek() {
                None | Some(0) => return Err(self.unrecognized()),
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
        while matches!(self.peek(), Some(c) if c != b'\'' && c != 0) {
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

    /// Lex a numeric literal — a port of tokenize.c's `CC_DIGIT` case (with the
    /// `CC_DOT` fall-through for `.5`-style floats) plus the digit-separator
    /// validation of `sqlite3DequoteNumber` (util.c, SQLite 3.46+ `_`
    /// separators).
    fn number(&mut self) -> Result<Token> {
        let start = self.pos;
        // `_` separator seen — tokenize.c's TK_QNUMBER; validated below.
        let mut saw_sep = false;
        let mut is_float = false;
        // Hex only when `0x` is followed by a hex digit (tokenize.c:
        // `z[0]=='0' && (z[1]=='x'||z[1]=='X') && sqlite3Isxdigit(z[2])`);
        // otherwise the `0` lexes as a decimal digit run and the `x…` becomes a
        // trailing identifier run, making the whole thing one bad token.
        let is_hex = self.peek() == Some(b'0')
            && matches!(self.peek_at(1), Some(b'x') | Some(b'X'))
            && matches!(self.peek_at(2), Some(c) if c.is_ascii_hexdigit());
        if is_hex {
            self.pos += 2;
            self.digit_run(|c| c.is_ascii_hexdigit(), &mut saw_sep);
        } else {
            self.digit_run(|c| c.is_ascii_digit(), &mut saw_sep);
            if self.peek() == Some(b'.') {
                is_float = true;
                self.pos += 1;
                self.digit_run(|c| c.is_ascii_digit(), &mut saw_sep);
            }
            // An exponent counts only when the `e`/`E` is followed by a digit
            // or a sign-then-digit; otherwise the `e` is left as a trailing
            // identifier character (so `1e+` is the bad token `1e`, the `+`
            // lexing separately — exactly sqlite's scan).
            if matches!(self.peek(), Some(b'e') | Some(b'E')) {
                let exp_ok = matches!(self.peek_at(1), Some(c) if c.is_ascii_digit())
                    || (matches!(self.peek_at(1), Some(b'+') | Some(b'-'))
                        && matches!(self.peek_at(2), Some(c) if c.is_ascii_digit()));
                if exp_ok {
                    is_float = true;
                    self.pos += 2; // the `e` and the sign or first digit
                    self.digit_run(|c| c.is_ascii_digit(), &mut saw_sep);
                }
            }
        }
        // A numeric literal immediately followed by an identifier character
        // (`123abc`, `0x1p4`, `12e3f`) is one "unrecognized token" spanning the
        // entire run — tokenize.c's trailing `while( IdChar(z[i]) )` loop.
        if matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
                self.pos += 1;
            }
            return Err(self.unrecognized());
        }
        let text = &self.src[start..self.pos];
        if saw_sep {
            self.check_separators(text, is_hex)?;
        }
        let text = if saw_sep {
            text.replace('_', "")
        } else {
            String::from(text)
        };
        if is_hex {
            // A hex run that overflows 64 bits is a *recognized* literal that
            // SQLite rejects with a dedicated message (echoing the literal with
            // any `_` separators already stripped, since `sqlite3DequoteNumber`
            // rewrites the token before `codeInteger` sees it). The error
            // carries the literal's offset — sqlite's `%#T` records the
            // expression offset, so its CLI carets the literal.
            let v = u64::from_str_radix(&text[2..], 16).map_err(|_| {
                Error::ParseAt(alloc::format!("hex literal too big: {text}"), start)
            })?;
            return Ok(Token::Integer(v as i64));
        }
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

    /// Advance over a run of digits accepted by `is_digit` and `_` separators
    /// (consumed unconditionally, like tokenize.c's TK_QNUMBER scan; whether
    /// each `_` actually sits between two digits is validated afterwards by
    /// [`Tokenizer::check_separators`]).
    fn digit_run(&mut self, is_digit: impl Fn(u8) -> bool, saw_sep: &mut bool) {
        loop {
            match self.peek() {
                Some(c) if is_digit(c) => self.pos += 1,
                Some(b'_') => {
                    *saw_sep = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
    }

    /// Port of `sqlite3DequoteNumber` (util.c): every `_` separator in a
    /// numeric literal must sit between two digits (hex digits for a hex
    /// literal). SQLite dequotes the token in place and, on a misplaced
    /// separator, echoes the buffer as it stands mid-rewrite — the
    /// already-dequoted prefix followed by the untouched remainder of the
    /// original text — so `1_2_` is reported as `unrecognized token: "122_"`.
    /// Reproduced exactly.
    fn check_separators(&self, text: &str, is_hex: bool) -> Result<()> {
        let b = text.as_bytes();
        let digit = |c: u8| {
            if is_hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        };
        for (i, &c) in b.iter().enumerate() {
            if c == b'_' && !(i > 0 && digit(b[i - 1]) && b.get(i + 1).copied().is_some_and(digit))
            {
                // The number of characters sqlite's write cursor has emitted:
                // every non-separator character before the offending `_` (all
                // earlier separators were valid, hence skipped).
                let kept = b[..i].iter().filter(|&&c| c != b'_').count();
                let mut shown = String::with_capacity(text.len());
                shown.extend(text[..i].chars().filter(|&c| c != '_'));
                shown.push_str(&text[kept..]);
                return Err(Error::Parse(alloc::format!(
                    "unrecognized token: \"{shown}\""
                )));
            }
        }
        Ok(())
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
                    // SQLite bounds `?N` to `1 ..= SQLITE_MAX_VARIABLE_NUMBER`
                    // (32766 by default); anything else — including a value too
                    // large for `u32` — is rejected at prepare time with this exact
                    // message (no byte position, unlike other lex errors).
                    match self.src[start..self.pos].parse::<u32>() {
                        Ok(n) if (1..=32766).contains(&n) => Ok(Token::Param(Param::Numbered(n))),
                        _ => Err(Error::Parse(
                            "variable number must be between ?1 and ?32766".into(),
                        )),
                    }
                }
            }
            _ => {
                // Port of tokenize.c CC_DOLLAR / CC_VARALPHA (the `$`, `@`, `:`
                // and `#` alphabetic-variable classes). The name is a run of
                // identifier characters, allowing embedded `::` pairs and a
                // trailing TCL-style `(...)` subscript. `n` counts the
                // identifier characters; a variable with none (a lone sigil) is
                // TK_ILLEGAL, exactly as sqlite's `if( n==0 )`.
                let start = self.pos;
                let mut n = 0usize;
                loop {
                    match self.peek() {
                        Some(c) if is_ident_continue(c) => {
                            n += 1;
                            self.pos += 1;
                        }
                        // `c=='(' && n>0`: a TCL variable subscript. Consume up
                        // to the matching `)`, stopping at whitespace or NUL —
                        // which leaves the token illegal (unterminated
                        // subscript).
                        Some(b'(') if n > 0 => {
                            self.pos += 1;
                            while matches!(self.peek(), Some(c) if c != 0 && !c.is_ascii_whitespace() && c != b')')
                            {
                                self.pos += 1;
                            }
                            if self.peek() == Some(b')') {
                                self.pos += 1;
                            } else {
                                return Err(self.unrecognized());
                            }
                            break;
                        }
                        // An embedded `::` pair (does not count toward `n`).
                        Some(b':') if self.peek_at(1) == Some(b':') => {
                            self.pos += 2;
                        }
                        _ => break,
                    }
                }
                if n == 0 {
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
    // SQLite's `IdChar`: alphanumerics, `_`, any high-bit byte — and, as an
    // undocumented compatibility feature (ticket #1066), `$`.
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
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
    use alloc::format;
    use alloc::string::ToString;
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
    }

    #[test]
    fn unterminated_block_comment_is_whitespace() {
        // tokenize.c CC_SLASH scans a `/*` comment to the terminator or the end
        // of input and still yields TK_COMMENT — an unterminated block comment
        // is trailing whitespace, not an error.
        assert_eq!(toks("/* nope"), vec![]);
        assert_eq!(
            toks("SELECT 1 /* trailing"),
            vec![Token::Word("SELECT".into()), Token::Integer(1)]
        );
        // A bare `/*` at the very end is instead a division operator followed
        // by a star (`z[1]!='*' || z[2]==0`).
        assert_eq!(toks("/*"), vec![Token::Slash, Token::Star]);
    }

    #[test]
    fn utf8_bom_is_whitespace() {
        // A UTF-8 BOM (EF BB BF) at a token boundary is skipped like
        // whitespace (tokenize.c CC_BOM), wherever it appears.
        assert_eq!(
            toks("\u{feff}SELECT 1"),
            vec![Token::Word("SELECT".into()), Token::Integer(1)]
        );
        assert_eq!(
            toks("SELECT \u{feff}1"),
            vec![Token::Word("SELECT".into()), Token::Integer(1)]
        );
        // Attached to a word it is an ordinary identifier byte instead
        // (IdChar ≥ 0x80): sqlite lexes `SELECT<BOM>` as one identifier.
        assert_eq!(
            toks("SELECT\u{feff} 1"),
            vec![Token::Word("SELECT\u{feff}".into()), Token::Integer(1)]
        );
        assert_eq!(toks("a\u{feff}b"), vec![Token::Word("a\u{feff}b".into())]);
    }

    #[test]
    fn nul_ends_the_input() {
        // SQL is a C string to sqlite: a NUL byte ends the input, silently
        // dropping whatever follows.
        assert_eq!(
            toks("SELECT 1\0 garbage'"),
            vec![Token::Word("SELECT".into()), Token::Integer(1),]
        );
        // Mid-token, the NUL ends the input too; an open literal is
        // unterminated exactly as at end-of-string.
        assert!(tokenize("SELECT 'ab\0cd'").is_err());
    }

    #[test]
    fn dollar_is_an_identifier_char() {
        // SQLite's IdChar includes `$` (ticket #1066), so `a$b` is one word
        // and a variable name may contain `$`, `::` pairs, and a TCL-style
        // `(...)` subscript.
        assert_eq!(toks("a$b"), vec![Token::Word("a$b".into())]);
        assert_eq!(
            toks("$a$b"),
            vec![Token::Param(Param::Named("$a$b".into()))]
        );
        assert_eq!(
            toks(":a::b"),
            vec![Token::Param(Param::Named(":a::b".into()))]
        );
        assert_eq!(
            toks("$x(1)"),
            vec![Token::Param(Param::Named("$x(1)".into()))]
        );
        assert_eq!(
            toks("#abc"),
            vec![Token::Param(Param::Named("#abc".into()))]
        );
        assert_eq!(toks("#12"), vec![Token::Param(Param::Named("#12".into()))]);
        // A number immediately followed by `$` is one bad token (`1$`).
        assert!(tokenize("1$").is_err());
        // An unterminated `$x(…` subscript is a bad token; a lone sigil too.
        assert!(tokenize("$x(abc").is_err());
        assert!(tokenize("$").is_err());
        assert!(tokenize("#").is_err());
    }

    #[test]
    fn digit_separator_dequote_error_text() {
        // A misplaced `_` separator is reported with sqlite's mid-dequote
        // buffer: the separator-stripped prefix + the untouched remainder
        // (sqlite3DequoteNumber rewrites the token in place).
        for (sql, tok) in [
            ("1_", "1_"),
            ("1__2", "1__2"),
            ("1_2_", "122_"),
            ("0x1_2_", "0x122_"),
            ("1_.5", "1_.5"),
            ("1._5", "1._5"),
            ("1e5_", "1e5_"),
        ] {
            match tokenize(sql) {
                Err(e) => assert_eq!(
                    e.to_string(),
                    format!("SQL error: unrecognized token: \"{tok}\""),
                    "for {sql}"
                ),
                Ok(t) => panic!("expected {sql} to fail, got {t:?}"),
            }
        }
        // Well-placed separators lex (including in hex and exponent runs).
        assert_eq!(toks("1_000"), vec![Token::Integer(1000)]);
        assert_eq!(toks("0x1_2"), vec![Token::Integer(18)]);
        assert_eq!(toks("1e5_3"), vec![Token::Float(1e53)]);
        assert_eq!(toks("1_2.3_4"), vec![Token::Float(12.34)]);
    }

    #[test]
    fn dangling_exponent_stops_before_the_sign() {
        // `1e+` (no digit after the sign) is the bad token `1e` — sqlite's
        // scan leaves the `e` as a trailing identifier character and the `+`
        // lexes separately.
        for (sql, tok) in [
            ("1e+", "1e"),
            ("1e-", "1e"),
            ("1.5e+", "1.5e"),
            ("1e", "1e"),
        ] {
            match tokenize(sql) {
                Err(e) => assert_eq!(
                    e.to_string(),
                    format!("SQL error: unrecognized token: \"{tok}\""),
                    "for {sql}"
                ),
                Ok(t) => panic!("expected {sql} to fail, got {t:?}"),
            }
        }
        assert_eq!(toks("5.e2"), vec![Token::Float(500.0)]);
    }

    #[test]
    fn utf8_identifier_preserved() {
        // A non-ASCII identifier should tokenize as a single word with its bytes
        // intact (bare words slice the source directly).
        assert_eq!(toks("café"), vec![Token::Word("café".into())]);
    }
}
