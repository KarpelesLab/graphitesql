//! SQLite's own regular-expression engine, ported byte-for-byte from
//! `ext/misc/regexp.c` (SQLite 3.50.4).
//!
//! This is **not** POSIX ERE or PCRE — it is SQLite's compact, purpose-built NFA
//! matcher, and the `regexp()` SQL function / `X REGEXP Y` operator are defined by
//! *this* engine's behavior. To stay byte-compatible with `sqlite3` we translate
//! that C faithfully rather than reimplement regex semantics from scratch.
//!
//! Supported syntax (from the reference file's header comment):
//!
//! * `X*` `X+` `X?` `X{p,q}` — repetition (`{p,q}` is expanded at compile time)
//! * `(X)` — grouping, `X|Y` — alternation
//! * `^` `$` — start/end anchors, `.` — any single character
//! * `[abc]` `[^abc]` `[a-z]` — character classes with ranges
//! * `\c` — C escapes (`\a \f \n \r \t \v`), metacharacter escapes, `\uXXXX`,
//!   `\xXX`, and the classes `\b \w \W \d \D \s \S`
//!
//! The search is **unanchored** by default (a match anywhere succeeds) unless the
//! pattern begins with `^`. There are no backreferences or lookaround. Matching is
//! an NFA simulation, so performance is bounded by `O(N*M)` and never exponential.

use alloc::vec::Vec;

/// The end-of-input character (also the argument of the `$` opcode).
const RE_EOF: i64 = 0;
/// Start-of-input sentinel — larger than any UTF-8 code point.
const RE_START: i64 = 0xfffffff;

// NFA opcodes (one "state" each), mirroring the `RE_OP_*` defines.
const RE_OP_MATCH: u8 = 1; // Match the one character in the argument
const RE_OP_ANY: u8 = 2; // Match any one character (".")
const RE_OP_ANYSTAR: u8 = 3; // Optimized ".*"
const RE_OP_FORK: u8 = 4; // Continue to both next and opcode at iArg
const RE_OP_GOTO: u8 = 5; // Jump to opcode at iArg
const RE_OP_ACCEPT: u8 = 6; // Halt with a successful match
const RE_OP_CC_INC: u8 = 7; // Beginning of a [...] character class
const RE_OP_CC_EXC: u8 = 8; // Beginning of a [^...] character class
const RE_OP_CC_VALUE: u8 = 9; // Single value in a character class
const RE_OP_CC_RANGE: u8 = 10; // Range of values in a character class
const RE_OP_WORD: u8 = 11; // Perl word character [A-Za-z0-9_]
const RE_OP_NOTWORD: u8 = 12; // Not a perl word character
const RE_OP_DIGIT: u8 = 13; // digit: [0-9]
const RE_OP_NOTDIGIT: u8 = 14; // Not a digit
const RE_OP_SPACE: u8 = 15; // space: [ \t\n\r\v\f]
const RE_OP_NOTSPACE: u8 = 16; // Not a space
const RE_OP_BOUNDARY: u8 = 17; // Boundary between word and non-word
const RE_OP_ATSTART: u8 = 18; // Currently at the start of the string

/// A compile-time error in the regular expression, carrying SQLite's exact
/// wording (e.g. `"unmatched '('"`).
pub type RegexError = &'static str;

/// A compiled regular expression: the opcode/argument arrays plus the optional
/// literal prefix used to fast-forward the unanchored search.
pub struct Regex {
    a_op: Vec<u8>,
    a_arg: Vec<i32>,
    z_init: [u8; 12],
    n_init: usize,
}

/// Extract the next Unicode character from `z[*i..mx]`, advancing `*i` past it.
/// Converts UTF-8 to a code point; malformed sequences yield `0xfffd`. Faithful
/// port of `re_next_char`.
fn re_next_char(z: &[u8], i: &mut usize, mx: usize) -> u32 {
    if *i >= mx {
        return 0;
    }
    let mut c = z[*i] as u32;
    *i += 1;
    if c >= 0x80 {
        if (c & 0xe0) == 0xc0 && *i < mx && (z[*i] & 0xc0) == 0x80 {
            c = (c & 0x1f) << 6 | (z[*i] as u32 & 0x3f);
            *i += 1;
            if c < 0x80 {
                c = 0xfffd;
            }
        } else if (c & 0xf0) == 0xe0
            && *i + 1 < mx
            && (z[*i] & 0xc0) == 0x80
            && (z[*i + 1] & 0xc0) == 0x80
        {
            c = (c & 0x0f) << 12 | ((z[*i] as u32 & 0x3f) << 6) | (z[*i + 1] as u32 & 0x3f);
            *i += 2;
            if c <= 0x7ff || (0xd800..=0xdfff).contains(&c) {
                c = 0xfffd;
            }
        } else if (c & 0xf8) == 0xf0
            && *i + 2 < mx
            && (z[*i] & 0xc0) == 0x80
            && (z[*i + 1] & 0xc0) == 0x80
            && (z[*i + 2] & 0xc0) == 0x80
        {
            c = (c & 0x07) << 18
                | ((z[*i] as u32 & 0x3f) << 12)
                | ((z[*i + 1] as u32 & 0x3f) << 6)
                | (z[*i + 2] as u32 & 0x3f);
            *i += 3;
            if c <= 0xffff || c > 0x10ffff {
                c = 0xfffd;
            }
        } else {
            c = 0xfffd;
        }
    }
    c
}

/// True if `c` is a perl "word" character: `[A-Za-z0-9_]`.
fn re_word_char(c: i64) -> bool {
    (b'0' as i64..=b'9' as i64).contains(&c)
        || (b'a' as i64..=b'z' as i64).contains(&c)
        || (b'A' as i64..=b'Z' as i64).contains(&c)
        || c == b'_' as i64
}

/// True if `c` is a digit `[0-9]`.
fn re_digit_char(c: i64) -> bool {
    (b'0' as i64..=b'9' as i64).contains(&c)
}

/// True if `c` is a perl "space" character `[ \t\r\n\v\f]`.
fn re_space_char(c: i64) -> bool {
    c == b' ' as i64
        || c == b'\t' as i64
        || c == b'\n' as i64
        || c == b'\r' as i64
        || c == 0x0b // \v
        || c == 0x0c // \f
}

/// Recognize a hex digit and, if so, fold it into `*v` (`*v = *v*16 + value`).
fn re_hex(c: u8, v: &mut i32) -> bool {
    let d = if c.is_ascii_digit() {
        (c - b'0') as i32
    } else if (b'a'..=b'f').contains(&c) {
        (c - b'a') as i32 + 10
    } else if (b'A'..=b'F').contains(&c) {
        (c - b'A') as i32 + 10
    } else {
        return false;
    };
    *v = *v * 16 + (d & 0xff);
    true
}

/// Add `state` to `set` if not already present (`re_add_state`, dedup).
fn re_add_state(set: &mut Vec<u16>, state: usize) {
    let s = state as u16;
    if !set.contains(&s) {
        set.push(s);
    }
}

/// The regex compiler state — mirrors the mutable half of `ReCompiled`/`ReInput`.
struct Compiler<'a> {
    z: &'a [u8],
    i: usize,
    mx: usize,
    nocase: bool,
    a_op: Vec<u8>,
    a_arg: Vec<i32>,
    z_err: Option<RegexError>,
}

impl<'a> Compiler<'a> {
    fn n_state(&self) -> usize {
        self.a_op.len()
    }

    /// `xNextChar`: next code point of the *pattern*, folded to lower case when
    /// case-insensitive.
    fn next_char(&mut self) -> u32 {
        let c = re_next_char(self.z, &mut self.i, self.mx);
        if self.nocase && (b'A' as u32..=b'Z' as u32).contains(&c) {
            c + (b'a' - b'A') as u32
        } else {
            c
        }
    }

    /// Peek the next raw byte of the pattern (0 at end) — `rePeek`.
    fn peek(&self) -> u8 {
        if self.i < self.mx { self.z[self.i] } else { 0 }
    }

    /// Insert an opcode just before `i_before` (`re_insert`); returns `i_before`.
    fn re_insert(&mut self, i_before: usize, op: u8, arg: i32) -> usize {
        self.a_op.insert(i_before, op);
        self.a_arg.insert(i_before, arg);
        i_before
    }

    /// Append an opcode at the end (`re_append`); returns its index.
    fn re_append(&mut self, op: u8, arg: i32) -> usize {
        self.re_insert(self.n_state(), op, arg)
    }

    /// Copy `n` opcodes starting at `i_start` onto the end (`re_copy`).
    fn re_copy(&mut self, i_start: usize, n: usize) {
        for k in 0..n {
            let op = self.a_op[i_start + k];
            let arg = self.a_arg[i_start + k];
            self.a_op.push(op);
            self.a_arg.push(arg);
        }
    }

    /// A backslash was seen; read and interpret the escaped character
    /// (`re_esc_char`). On an unknown escape, sets `z_err` and returns the byte.
    fn esc_char(&mut self) -> u32 {
        // The metacharacters that may be backslash-escaped; the first six map to
        // C control characters.
        const ZESC: &[u8] = b"afnrtv\\()*.+?[$^{|}]";
        const ZTRANS: &[u8] = b"\x07\x0c\x0a\x0d\x09\x0b"; // \a \f \n \r \t \v
        if self.i >= self.mx {
            return 0;
        }
        let c0 = self.z[self.i];
        if c0 == b'u' && self.i + 4 < self.mx {
            let base = self.i;
            let mut v: i32 = 0;
            if re_hex(self.z[base + 1], &mut v)
                && re_hex(self.z[base + 2], &mut v)
                && re_hex(self.z[base + 3], &mut v)
                && re_hex(self.z[base + 4], &mut v)
            {
                self.i += 5;
                return v as u32;
            }
        }
        if c0 == b'x' && self.i + 2 < self.mx {
            let base = self.i;
            let mut v: i32 = 0;
            if re_hex(self.z[base + 1], &mut v) && re_hex(self.z[base + 2], &mut v) {
                self.i += 3;
                return v as u32;
            }
        }
        let mut idx = 0;
        while idx < ZESC.len() && ZESC[idx] != c0 {
            idx += 1;
        }
        if idx < ZESC.len() {
            let c = if idx < 6 { ZTRANS[idx] } else { c0 };
            self.i += 1;
            c as u32
        } else {
            self.z_err = Some("unknown \\ escape");
            c0 as u32
        }
    }

    /// Compile up to the first unmatched `)` — `re_subcompile_re` (handles `|`).
    fn subcompile_re(&mut self) -> Result<(), RegexError> {
        let i_start = self.n_state();
        self.subcompile_string()?;
        while self.peek() == b'|' {
            let i_end = self.n_state();
            self.re_insert(i_start, RE_OP_FORK, (i_end + 2 - i_start) as i32);
            let i_goto = self.re_append(RE_OP_GOTO, 0);
            self.i += 1;
            self.subcompile_string()?;
            self.a_arg[i_goto] = (self.n_state() - i_goto) as i32;
        }
        Ok(())
    }

    /// Compile one alternation operand — `re_subcompile_string`.
    fn subcompile_string(&mut self) -> Result<(), RegexError> {
        let mut i_prev: i64 = -1;
        loop {
            let c = self.next_char();
            if c == 0 {
                break;
            }
            let i_start = self.n_state();
            match c {
                c if c == b'|' as u32 || c == b')' as u32 => {
                    self.i -= 1;
                    return Ok(());
                }
                c if c == b'(' as u32 => {
                    self.subcompile_re()?;
                    if self.peek() != b')' {
                        return Err("unmatched '('");
                    }
                    self.i += 1;
                }
                c if c == b'.' as u32 => {
                    if self.peek() == b'*' {
                        self.re_append(RE_OP_ANYSTAR, 0);
                        self.i += 1;
                    } else {
                        self.re_append(RE_OP_ANY, 0);
                    }
                }
                c if c == b'*' as u32 => {
                    if i_prev < 0 {
                        return Err("'*' without operand");
                    }
                    let n = self.n_state() as i64;
                    self.re_insert(i_prev as usize, RE_OP_GOTO, (n - i_prev + 1) as i32);
                    let n2 = self.n_state() as i64;
                    self.re_append(RE_OP_FORK, (i_prev - n2 + 1) as i32);
                }
                c if c == b'+' as u32 => {
                    if i_prev < 0 {
                        return Err("'+' without operand");
                    }
                    let n = self.n_state() as i64;
                    self.re_append(RE_OP_FORK, (i_prev - n) as i32);
                }
                c if c == b'?' as u32 => {
                    if i_prev < 0 {
                        return Err("'?' without operand");
                    }
                    let n = self.n_state() as i64;
                    self.re_insert(i_prev as usize, RE_OP_FORK, (n - i_prev + 1) as i32);
                }
                c if c == b'$' as u32 => {
                    self.re_append(RE_OP_MATCH, RE_EOF as i32);
                }
                c if c == b'^' as u32 => {
                    self.re_append(RE_OP_ATSTART, 0);
                }
                c if c == b'{' as u32 => {
                    if i_prev < 0 {
                        return Err("'{m,n}' without operand");
                    }
                    let mut m: i64 = 0;
                    let mut cc = self.peek();
                    while cc.is_ascii_digit() {
                        m = m * 10 + (cc - b'0') as i64;
                        self.i += 1;
                        cc = self.peek();
                    }
                    let mut n = m;
                    if cc == b',' {
                        self.i += 1;
                        n = 0;
                        cc = self.peek();
                        while cc.is_ascii_digit() {
                            n = n * 10 + (cc - b'0') as i64;
                            self.i += 1;
                            cc = self.peek();
                        }
                    }
                    if cc != b'}' {
                        return Err("unmatched '{'");
                    }
                    if n > 0 && n < m {
                        return Err("n less than m in '{m,n}'");
                    }
                    self.i += 1;
                    let sz = self.n_state() as i64 - i_prev;
                    let mut i_prev_q = i_prev;
                    if m == 0 {
                        if n == 0 {
                            return Err("both m and n are zero in '{m,n}'");
                        }
                        self.re_insert(i_prev_q as usize, RE_OP_FORK, (sz + 1) as i32);
                        i_prev_q += 1;
                        n -= 1;
                    } else {
                        let mut j = 1;
                        while j < m {
                            self.re_copy(i_prev_q as usize, sz as usize);
                            j += 1;
                        }
                    }
                    let mut j = m;
                    while j < n {
                        self.re_append(RE_OP_FORK, (sz + 1) as i32);
                        self.re_copy(i_prev_q as usize, sz as usize);
                        j += 1;
                    }
                    if n == 0 && m > 0 {
                        self.re_append(RE_OP_FORK, (-sz) as i32);
                    }
                }
                c if c == b'[' as u32 => {
                    let i_first = self.n_state();
                    if self.peek() == b'^' {
                        self.re_append(RE_OP_CC_EXC, 0);
                        self.i += 1;
                    } else {
                        self.re_append(RE_OP_CC_INC, 0);
                    }
                    let mut cch;
                    loop {
                        cch = self.next_char();
                        if cch == 0 {
                            break;
                        }
                        if cch == b'[' as u32 && self.peek() == b':' {
                            return Err("POSIX character classes not supported");
                        }
                        if cch == b'\\' as u32 {
                            cch = self.esc_char();
                        }
                        if self.peek() == b'-' {
                            self.re_append(RE_OP_CC_RANGE, cch as i32);
                            self.i += 1;
                            cch = self.next_char();
                            if cch == b'\\' as u32 {
                                cch = self.esc_char();
                            }
                            self.re_append(RE_OP_CC_RANGE, cch as i32);
                        } else {
                            self.re_append(RE_OP_CC_VALUE, cch as i32);
                        }
                        if self.peek() == b']' {
                            self.i += 1;
                            break;
                        }
                    }
                    if cch == 0 {
                        return Err("unclosed '['");
                    }
                    if self.n_state() > i_first {
                        self.a_arg[i_first] = (self.n_state() - i_first) as i32;
                    }
                }
                c if c == b'\\' as u32 => {
                    let special_op = match self.peek() {
                        b'b' => RE_OP_BOUNDARY,
                        b'd' => RE_OP_DIGIT,
                        b'D' => RE_OP_NOTDIGIT,
                        b's' => RE_OP_SPACE,
                        b'S' => RE_OP_NOTSPACE,
                        b'w' => RE_OP_WORD,
                        b'W' => RE_OP_NOTWORD,
                        _ => 0,
                    };
                    if special_op != 0 {
                        self.i += 1;
                        self.re_append(special_op, 0);
                    } else {
                        let ec = self.esc_char();
                        self.re_append(RE_OP_MATCH, ec as i32);
                    }
                }
                _ => {
                    self.re_append(RE_OP_MATCH, c as i32);
                }
            }
            i_prev = i_start as i64;
        }
        Ok(())
    }
}

/// Compile a textual regular expression into a [`Regex`], or return SQLite's
/// exact error string. `nocase` selects the case-insensitive variant
/// (`regexpi`); the default `regexp` is case-sensitive. Faithful port of
/// `re_compile`.
pub fn compile(pattern: &[u8], nocase: bool) -> Result<Regex, RegexError> {
    // `re_compile` operates on a C string, so the pattern is truncated at the
    // first NUL byte (its `strlen`).
    let end = pattern
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(pattern.len());
    let mut z = &pattern[..end];

    let mut c = Compiler {
        z: &[],
        i: 0,
        mx: 0,
        nocase,
        a_op: Vec::new(),
        a_arg: Vec::new(),
        z_err: None,
    };

    if !z.is_empty() && z[0] == b'^' {
        z = &z[1..];
    } else {
        c.re_append(RE_OP_ANYSTAR, 0);
    }
    c.z = z;
    c.i = 0;
    c.mx = z.len();

    c.subcompile_re()?;

    if c.i >= c.mx {
        c.re_append(RE_OP_ACCEPT, 0);
    } else {
        return Err("unrecognized character");
    }

    // Performance optimization: if the regex begins with ".*" (no leading "^")
    // followed by literal matches, record the literal prefix in z_init so the
    // search can fast-forward without running the whole engine.
    let mut z_init = [0u8; 12];
    let mut n_init = 0usize;
    if c.a_op[0] == RE_OP_ANYSTAR && !nocase {
        let mut j = 0usize;
        let mut i = 1usize;
        while j < 12 - 2 && i < c.a_op.len() && c.a_op[i] == RE_OP_MATCH {
            let x = c.a_arg[i] as u32;
            if x <= 0x7f {
                z_init[j] = x as u8;
                j += 1;
            } else if x <= 0x7ff {
                z_init[j] = (0xc0 | (x >> 6)) as u8;
                j += 1;
                z_init[j] = (0x80 | (x & 0x3f)) as u8;
                j += 1;
            } else if x <= 0xffff {
                z_init[j] = (0xe0 | (x >> 12)) as u8;
                j += 1;
                z_init[j] = (0x80 | ((x >> 6) & 0x3f)) as u8;
                j += 1;
                z_init[j] = (0x80 | (x & 0x3f)) as u8;
                j += 1;
            } else {
                break;
            }
            i += 1;
        }
        if j > 0 && z_init[j - 1] == 0 {
            j -= 1;
        }
        n_init = j;
    }

    // A deferred `re_esc_char` error surfaces here (as `re_compile` does).
    if let Some(e) = c.z_err {
        return Err(e);
    }

    Ok(Regex {
        a_op: c.a_op,
        a_arg: c.a_arg,
        z_init,
        n_init,
    })
}

impl Regex {
    /// Next input code point during matching, folded to lower case when the regex
    /// is case-insensitive (`xNextChar`).
    fn next_input_char(&self, z: &[u8], i: &mut usize, mx: usize, nocase: bool) -> u32 {
        let c = re_next_char(z, i, mx);
        if nocase && (b'A' as u32..=b'Z' as u32).contains(&c) {
            c + (b'a' - b'A') as u32
        } else {
            c
        }
    }

    /// Run the compiled regex over `z_in`, returning whether it matches.
    /// Faithful port of `re_match`. `nocase` must match the value passed to
    /// [`compile`].
    fn matches(&self, z_in: &[u8], nocase: bool) -> bool {
        // `re_match` is called with nIn = -1, so the subject is a C string and
        // ends at the first NUL byte (its `strlen`).
        let mx = z_in.iter().position(|&b| b == 0).unwrap_or(z_in.len());
        let z = &z_in[..mx];

        let mut in_i = 0usize;
        let mut c: i64 = RE_START;
        let mut c_prev: i64;
        let mut rc = false;

        // Look for the initial literal prefix, if there is one.
        if self.n_init > 0 {
            let x = self.z_init[0];
            while in_i + self.n_init <= mx
                && (z[in_i] != x || z[in_i..in_i + self.n_init] != self.z_init[..self.n_init])
            {
                in_i += 1;
            }
            if in_i + self.n_init > mx {
                return false;
            }
            c = RE_START - 1;
        }

        // Two alternating state sets (double buffer). `next_buf` is `pNext`.
        let mut sets: [Vec<u16>; 2] = [Vec::new(), Vec::new()];
        let mut next_buf = 1usize;
        re_add_state(&mut sets[next_buf], 0);

        'outer: while c != RE_EOF && !sets[next_buf].is_empty() {
            c_prev = c;
            c = self.next_input_char(z, &mut in_i, mx, nocase) as i64;
            let this_buf = next_buf;
            next_buf = 1 - this_buf;
            sets[next_buf].clear();

            let mut idx = 0;
            while idx < sets[this_buf].len() {
                let x = sets[this_buf][idx] as usize;
                match self.a_op[x] {
                    RE_OP_MATCH => {
                        if self.a_arg[x] as i64 == c {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_ATSTART => {
                        if c_prev == RE_START {
                            re_add_state(&mut sets[this_buf], x + 1);
                        }
                    }
                    RE_OP_ANY => {
                        if c != 0 {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_WORD => {
                        if re_word_char(c) {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_NOTWORD => {
                        if !re_word_char(c) && c != 0 {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_DIGIT => {
                        if re_digit_char(c) {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_NOTDIGIT => {
                        if !re_digit_char(c) && c != 0 {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_SPACE => {
                        if re_space_char(c) {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_NOTSPACE => {
                        if !re_space_char(c) && c != 0 {
                            re_add_state(&mut sets[next_buf], x + 1);
                        }
                    }
                    RE_OP_BOUNDARY => {
                        if re_word_char(c) != re_word_char(c_prev) {
                            re_add_state(&mut sets[this_buf], x + 1);
                        }
                    }
                    RE_OP_ANYSTAR => {
                        re_add_state(&mut sets[next_buf], x);
                        re_add_state(&mut sets[this_buf], x + 1);
                    }
                    RE_OP_FORK => {
                        let target = (x as i64 + self.a_arg[x] as i64) as usize;
                        re_add_state(&mut sets[this_buf], target);
                        re_add_state(&mut sets[this_buf], x + 1);
                    }
                    RE_OP_GOTO => {
                        let target = (x as i64 + self.a_arg[x] as i64) as usize;
                        re_add_state(&mut sets[this_buf], target);
                    }
                    RE_OP_ACCEPT => {
                        rc = true;
                        break 'outer;
                    }
                    RE_OP_CC_INC | RE_OP_CC_EXC => {
                        let is_exc = self.a_op[x] == RE_OP_CC_EXC;
                        if is_exc && c == 0 {
                            // [^...] never matches end-of-input.
                        } else {
                            let n = self.a_arg[x] as i64;
                            let mut hit = false;
                            let mut j: i64 = 1;
                            while j > 0 && j < n {
                                let xi = (x as i64 + j) as usize;
                                if self.a_op[xi] == RE_OP_CC_VALUE {
                                    if self.a_arg[xi] as i64 == c {
                                        hit = true;
                                        j = -1;
                                    }
                                } else if (self.a_arg[xi] as i64) <= c
                                    && (self.a_arg[xi + 1] as i64) >= c
                                {
                                    hit = true;
                                    j = -1;
                                } else {
                                    j += 1;
                                }
                                j += 1;
                            }
                            if is_exc {
                                hit = !hit;
                            }
                            if hit {
                                re_add_state(&mut sets[next_buf], (x as i64 + n) as usize);
                            }
                        }
                    }
                    _ => {}
                }
                idx += 1;
            }
        }

        if rc {
            return true;
        }

        // The loop ended without an explicit ACCEPT: a state may still reach
        // ACCEPT through GOTOs at end-of-input.
        for &state in &sets[next_buf] {
            let mut x = state as usize;
            while self.a_op[x] == RE_OP_GOTO {
                x = (x as i64 + self.a_arg[x] as i64) as usize;
            }
            if self.a_op[x] == RE_OP_ACCEPT {
                return true;
            }
        }
        false
    }
}

/// Compile `pattern` and test whether it matches `subject`, mirroring SQLite's
/// case-sensitive `regexp(pattern, subject)` (and thus `subject REGEXP pattern`).
/// Returns SQLite's exact error string for an invalid pattern.
pub fn regexp_match(pattern: &[u8], subject: &[u8]) -> Result<bool, RegexError> {
    let re = compile(pattern, false)?;
    Ok(re.matches(subject, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, subj: &str) -> bool {
        regexp_match(pat.as_bytes(), subj.as_bytes()).unwrap()
    }

    #[test]
    fn dot_matches_any_single() {
        assert!(m("a.c", "abc"));
        assert!(m("a.c", "axc"));
        assert!(!m("a.c", "ac"));
        // Unanchored: matches anywhere.
        assert!(m("a.c", "zzabczz"));
    }

    #[test]
    fn anchors() {
        assert!(m("^abc$", "abc"));
        assert!(!m("^abc$", "xabc"));
        assert!(!m("^abc$", "abcx"));
        assert!(m("^abc", "abcdef"));
        assert!(m("abc$", "xxabc"));
        assert!(!m("^abc", "xabc"));
    }

    #[test]
    fn star_plus_question() {
        assert!(m("a*b", "b"));
        assert!(m("a*b", "aaab"));
        assert!(m("a+", "baa"));
        assert!(!m("^a+$", "b"));
        assert!(m("^colou?r$", "color"));
        assert!(m("^colou?r$", "colour"));
        assert!(!m("^colou?r$", "colouur"));
    }

    #[test]
    fn dotstar() {
        assert!(m(".*", ""));
        assert!(m(".*", "anything"));
        assert!(m("a.*z", "a___z"));
        assert!(!m("^a.*z$", "a__zx"));
    }

    #[test]
    fn char_classes() {
        assert!(m("[a-z]+", "abc"));
        assert!(!m("^[a-z]+$", "abc1"));
        assert!(m("[^0-9]", "a"));
        assert!(!m("^[^0-9]$", "5"));
        assert!(m("[abc]", "cxx"));
    }

    #[test]
    fn braces() {
        assert!(m("^a{2,3}$", "aa"));
        assert!(m("^a{2,3}$", "aaa"));
        assert!(!m("^a{2,3}$", "a"));
        assert!(!m("^a{2,3}$", "aaaa"));
        assert!(m("^a{2}$", "aa"));
        assert!(m("^a{2,}$", "aaaaa"));
        assert!(!m("^a{0,2}b$", "aaab"));
    }

    #[test]
    fn alternation_and_groups() {
        assert!(m("^(foo|bar)$", "foo"));
        assert!(m("^(foo|bar)$", "bar"));
        assert!(!m("^(foo|bar)$", "baz"));
        assert!(m("(ab)+", "abab"));
    }

    #[test]
    fn escape_classes() {
        assert!(m("\\d+", "abc123"));
        assert!(!m("^\\d+$", "12a"));
        assert!(m("^\\w+$", "hello_9"));
        assert!(!m("^\\w+$", "a b"));
        assert!(m("\\s", "a b"));
        assert!(m("^\\D+$", "abc"));
        assert!(m("^\\S+$", "abc"));
    }

    #[test]
    fn word_boundary() {
        assert!(m("\\bcat\\b", "the cat sat"));
        assert!(!m("\\bcat\\b", "category"));
    }

    #[test]
    fn escapes_and_hex() {
        assert!(m("^a\\.c$", "a.c"));
        assert!(!m("^a\\.c$", "abc"));
        assert!(m("^\\x41$", "A"));
        assert!(m("^\\u0041$", "A"));
        assert!(m("^\\t$", "\t"));
    }

    #[test]
    fn empty_pattern_matches_everything() {
        assert!(m("", ""));
        assert!(m("", "anything"));
    }

    #[test]
    fn unicode() {
        assert!(m("^.$", "é"));
        assert!(m("café", "a café here"));
    }

    #[test]
    fn errors() {
        assert_eq!(compile(b"(", false).err(), Some("unmatched '('"));
        assert_eq!(compile(b"*", false).err(), Some("'*' without operand"));
        assert_eq!(compile(b"[abc", false).err(), Some("unclosed '['"));
        assert_eq!(
            compile(b"a{3,2}", false).err(),
            Some("n less than m in '{m,n}'")
        );
        assert_eq!(compile(b"\\q", false).err(), Some("unknown \\ escape"));
        assert!(compile(b"a.c", false).is_ok());
    }
}
