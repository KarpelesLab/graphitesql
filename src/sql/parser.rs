//! A hand-written recursive-descent parser with a Pratt expression core.
//!
//! It turns a token stream into [`Statement`]s. The grammar source of truth is
//! SQLite's `parse.y`; this implements the commonly-used core and grows toward
//! it. Precedence follows SQLite's operator table (`lang_expr.html`): from
//! loosest to tightest — `OR`, `AND`, `NOT`, comparison/`IS`/`IN`/`LIKE`/
//! `BETWEEN`, relational, bitwise, additive, multiplicative, `||`, then unary.

use crate::error::{Error, Result};
use crate::sql::ast::*;
use crate::sql::token::{tokenize, Spanned, Token};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

// Binding powers (higher binds tighter).
const BP_OR: u8 = 10;
const BP_AND: u8 = 20;
const BP_NOT_PREFIX: u8 = 35;
const BP_EQ: u8 = 40; // = != IS IN LIKE GLOB BETWEEN
const BP_REL: u8 = 50; // < <= > >=
const BP_BIT: u8 = 60; // & | << >>
const BP_ADD: u8 = 70;
const BP_MUL: u8 = 80;
const BP_CONCAT: u8 = 90;
const BP_UNARY: u8 = 100;

/// Parse a SQL string into a list of statements (split on `;`).
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser::new(tokens);
    let mut statements = Vec::new();
    loop {
        while parser.eat(&Token::Semicolon) {}
        if parser.at_end() {
            break;
        }
        statements.push(parser.statement()?);
        if !parser.at_end() && !parser.check(&Token::Semicolon) {
            return Err(parser.err("expected ';' or end of input after statement"));
        }
    }
    Ok(statements)
}

/// Parse exactly one statement, erroring if there is trailing input.
pub fn parse_one(sql: &str) -> Result<Statement> {
    let mut stmts = parse(sql)?;
    match stmts.len() {
        1 => Ok(stmts.pop().unwrap()),
        0 => Err(Error::Parse("empty statement".into())),
        _ => Err(Error::Parse("expected a single statement".into())),
    }
}

/// Render one token of a `CREATE VIRTUAL TABLE … USING m(…)` argument back to
/// the string a virtual-table module receives. Module arguments are not SQL
/// expressions — SQLite hands them over verbatim — so this reproduces the
/// literal's value (e.g. `Integer(5)` → `"5"`, `Str("a")` → `"a"`).
fn token_arg_text(tok: &Token) -> String {
    match tok {
        Token::Word(w) => w.clone(),
        Token::Ident(i) => i.clone(),
        Token::Integer(n) => alloc::format!("{n}"),
        Token::Int2Pow63 => String::from("9223372036854775808"),
        Token::Float(f) => alloc::format!("{f}"),
        Token::Str(s) => s.clone(),
        Token::Minus => String::from("-"),
        Token::Plus => String::from("+"),
        Token::Dot => String::from("."),
        Token::Star => String::from("*"),
        Token::Eq => String::from("="),
        other => alloc::format!("{other:?}"),
    }
}

/// Whether `s` ends in an alphanumeric/underscore character (a word-like run
/// that should be space-separated from a following word-like token).
fn ends_wordish(s: &str) -> bool {
    s.chars().next_back().is_some_and(is_wordish_start)
}

/// Whether `c` begins a word-like token (letter, digit, or underscore).
fn is_wordish_start(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Maximum recursion depth for nested expressions and sub-selects.
///
/// The parser is recursive descent, so an attacker-supplied string with very
/// deep nesting (e.g. hundreds of `(`, `NOT`, or `CASE`) would otherwise
/// overflow the native stack and abort the process. SQLite caps the same way
/// via `SQLITE_MAX_EXPR_DEPTH` (default 1000), but its parse frames are tiny C
/// frames; ours carry large `Expr`/`Select` values by value, so each level
/// costs far more stack. We therefore cap well below the point where parsing
/// would overflow even a small (2 MiB) thread stack, while still admitting far
/// more nesting than any realistic query needs. Past the limit, parsing fails
/// cleanly with [`Error::Parse`] instead of crashing the process.
///
/// The cap also protects the downstream binder/planner/evaluator, which recurse
/// over the parsed AST and (for nested sub-selects especially) use even more
/// stack per level than the parser does.
const MAX_PARSE_DEPTH: usize = 30;

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
    /// Current recursive-descent nesting depth (guarded by [`MAX_PARSE_DEPTH`]).
    ///
    /// Shared via [`Rc`] so a [`DepthGuard`] can decrement it on drop without
    /// borrowing the parser, which stays mutably borrowed during recursion.
    depth: alloc::rc::Rc<core::cell::Cell<usize>>,
}

/// Decrements the parser's depth counter when dropped, so recursion accounting
/// stays balanced across every exit path (including the `?` operator).
struct DepthGuard {
    depth: alloc::rc::Rc<core::cell::Cell<usize>>,
}

impl core::ops::Drop for DepthGuard {
    fn drop(&mut self) {
        self.depth.set(self.depth.get() - 1);
    }
}

impl Parser {
    fn new(tokens: Vec<Spanned>) -> Parser {
        Parser {
            tokens,
            pos: 0,
            depth: alloc::rc::Rc::new(core::cell::Cell::new(0)),
        }
    }

    /// Enter one level of recursion, erroring if the nesting limit is exceeded.
    /// The returned guard decrements the counter when dropped, so every exit
    /// path (including `?`) is balanced.
    fn enter(&self) -> Result<DepthGuard> {
        let d = self.depth.get();
        if d >= MAX_PARSE_DEPTH {
            return Err(Error::Parse("expression or query nesting too deep".into()));
        }
        self.depth.set(d + 1);
        Ok(DepthGuard {
            depth: alloc::rc::Rc::clone(&self.depth),
        })
    }

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|s| &s.token)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).map(|s| s.token.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn check(&self, t: &Token) -> bool {
        self.peek() == Some(t)
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.check(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Token) -> Result<()> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {t:?}")))
        }
    }

    fn err(&self, msg: &str) -> Error {
        match self.tokens.get(self.pos) {
            Some(s) => Error::Parse(format!(
                "{msg} (near byte {}, found {:?})",
                s.start, s.token
            )),
            None => Error::Parse(format!("{msg} (at end of input)")),
        }
    }

    /// Is the current token the keyword `kw` (case-insensitive bare word)?
    fn check_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    /// Consume the keyword `kw` if present.
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.check_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err(&format!("expected keyword {kw}")))
        }
    }

    /// Parse an identifier (a bare word or a quoted identifier).
    fn ident(&mut self) -> Result<String> {
        match self.advance() {
            Some(Token::Word(w)) => Ok(w),
            Some(Token::Ident(i)) => Ok(i),
            other => Err(Error::Parse(format!(
                "expected identifier, found {other:?}"
            ))),
        }
    }

    // ---- statements ---------------------------------------------------------

    fn statement(&mut self) -> Result<Statement> {
        if self.eat_kw("explain") {
            let query_plan = if self.eat_kw("query") {
                if !self.eat_kw("plan") {
                    return Err(self.err("expected PLAN after EXPLAIN QUERY"));
                }
                true
            } else {
                false
            };
            let stmt = self.statement()?;
            return Ok(Statement::Explain {
                query_plan,
                stmt: alloc::boxed::Box::new(stmt),
            });
        }
        if self.check_kw("select") || self.check_kw("with") || self.check_kw("values") {
            return Ok(Statement::Select(self.select()?));
        }
        if self.check_kw("insert") || self.check_kw("replace") {
            return Ok(Statement::Insert(self.insert()?));
        }
        if self.check_kw("update") {
            return Ok(Statement::Update(self.update()?));
        }
        if self.check_kw("delete") {
            return Ok(Statement::Delete(self.delete()?));
        }
        if self.check_kw("create") {
            return self.create();
        }
        if self.check_kw("drop") {
            return Ok(Statement::Drop(self.drop_stmt()?));
        }
        if self.check_kw("alter") {
            return Ok(Statement::Alter(self.alter()?));
        }
        if self.eat_kw("begin") {
            let _ = self.eat_kw("transaction")
                || self.eat_kw("deferred")
                || self.eat_kw("immediate")
                || self.eat_kw("exclusive");
            let _ = self.eat_kw("transaction");
            return Ok(Statement::Begin);
        }
        if self.eat_kw("commit") || self.eat_kw("end") {
            let _ = self.eat_kw("transaction");
            return Ok(Statement::Commit);
        }
        if self.eat_kw("rollback") {
            let _ = self.eat_kw("transaction");
            if self.eat_kw("to") {
                let _ = self.eat_kw("savepoint");
                return Ok(Statement::RollbackTo(self.ident()?));
            }
            return Ok(Statement::Rollback);
        }
        if self.eat_kw("savepoint") {
            return Ok(Statement::Savepoint(self.ident()?));
        }
        if self.eat_kw("release") {
            let _ = self.eat_kw("savepoint");
            return Ok(Statement::Release(self.ident()?));
        }
        if self.eat_kw("pragma") {
            return Ok(Statement::Pragma(self.pragma()?));
        }
        if self.eat_kw("vacuum") {
            // Accept `VACUUM [schema] [INTO 'file']`; the clauses don't affect us.
            if !self.check(&Token::Semicolon) && !self.at_end() && !self.eat_kw("into") {
                let _ = self.advance(); // optional schema name
            }
            if self.eat_kw("into") {
                let _ = self.expr()?;
            }
            return Ok(Statement::Vacuum);
        }
        if self.eat_kw("reindex") {
            // `REINDEX` / `REINDEX name` / `REINDEX schema.name` (a no-op here).
            if !self.check(&Token::Semicolon) && !self.at_end() {
                let _ = self.ident()?;
                if self.eat(&Token::Dot) {
                    let _ = self.ident()?;
                }
            }
            return Ok(Statement::Reindex);
        }
        if self.eat_kw("analyze") {
            // `ANALYZE` / `ANALYZE name` / `ANALYZE schema.name`.
            let target = if self.check(&Token::Semicolon) || self.at_end() {
                None
            } else {
                let mut name = self.ident()?;
                if self.eat(&Token::Dot) {
                    name = self.ident()?; // schema-qualified: keep the object name
                }
                Some(name)
            };
            return Ok(Statement::Analyze(target));
        }
        if self.eat_kw("attach") {
            // `ATTACH [DATABASE] <expr> AS <name>`.
            let _ = self.eat_kw("database");
            let file = self.expr()?;
            self.expect_kw("as")?;
            let name = self.ident()?;
            return Ok(Statement::Attach { file, name });
        }
        if self.eat_kw("detach") {
            // `DETACH [DATABASE] <name>`.
            let _ = self.eat_kw("database");
            let name = self.ident()?;
            return Ok(Statement::Detach(name));
        }
        Err(self.err("unrecognized statement"))
    }

    fn pragma(&mut self) -> Result<Pragma> {
        let name = self.ident()?;
        let value = if self.eat(&Token::Eq) {
            Some(self.pragma_value()?)
        } else if self.eat(&Token::LParen) {
            let v = self.pragma_value()?;
            self.expect(&Token::RParen)?;
            Some(v)
        } else {
            None
        };
        Ok(Pragma { name, value })
    }

    /// A PRAGMA argument: a normal expression, but also a bare keyword like
    /// `ON`/`OFF`/`FULL` that SQLite accepts as a literal here.
    fn pragma_value(&mut self) -> Result<Expr> {
        if let Some(Token::Word(w)) = self.peek() {
            let w = w.clone();
            // A bare word not followed by an operator/paren is a keyword literal.
            if is_reserved_keyword(&w.to_ascii_lowercase()) {
                self.pos += 1;
                return Ok(Expr::Column {
                    table: None,
                    column: w,
                });
            }
        }
        self.expr()
    }

    fn select(&mut self) -> Result<Select> {
        let _guard = self.enter()?;
        let mut ctes = Vec::new();
        if self.eat_kw("with") {
            // `RECURSIVE` is accepted as a keyword but recursion is not yet run.
            let _ = self.eat_kw("recursive");
            loop {
                let name = self.ident()?;
                let mut columns = Vec::new();
                if self.eat(&Token::LParen) {
                    columns.push(self.ident()?);
                    while self.eat(&Token::Comma) {
                        columns.push(self.ident()?);
                    }
                    self.expect(&Token::RParen)?;
                }
                self.expect_kw("as")?;
                self.expect(&Token::LParen)?;
                let select = Box::new(self.select()?);
                self.expect(&Token::RParen)?;
                ctes.push(Cte {
                    name,
                    columns,
                    select,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        // First query core, then any compound continuations (left-associative).
        let mut outer = self.select_core()?;
        outer.ctes = ctes;
        while let Some(op) = self.compound_op() {
            let right = self.select_core()?;
            outer.compound.push((op, right));
        }

        // Trailing ORDER BY / LIMIT / OFFSET apply to the whole (compound) query.
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            outer.order_by.push(self.order_term()?);
            while self.eat(&Token::Comma) {
                outer.order_by.push(self.order_term()?);
            }
        }
        if self.eat_kw("limit") {
            outer.limit = Some(self.expr()?);
            if self.eat_kw("offset") {
                outer.offset = Some(self.expr()?);
            } else if self.eat(&Token::Comma) {
                // `LIMIT offset, count` form.
                outer.offset = outer.limit.take();
                outer.limit = Some(self.expr()?);
            }
        }
        Ok(outer)
    }

    /// Parse a trailing `[ORDER BY …] [LIMIT … [OFFSET …| , …]]`, returning the
    /// terms, limit, and offset. Shared by `SELECT` and the `UPDATE`/`DELETE`
    /// row-limit extension.
    fn order_limit_offset(&mut self) -> Result<(Vec<OrderTerm>, Option<Expr>, Option<Expr>)> {
        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            order_by.push(self.order_term()?);
            while self.eat(&Token::Comma) {
                order_by.push(self.order_term()?);
            }
        }
        let mut limit = None;
        let mut offset = None;
        if self.eat_kw("limit") {
            limit = Some(self.expr()?);
            if self.eat_kw("offset") {
                offset = Some(self.expr()?);
            } else if self.eat(&Token::Comma) {
                offset = limit.take();
                limit = Some(self.expr()?);
            }
        }
        Ok((order_by, limit, offset))
    }

    /// A `UNION [ALL]` / `INTERSECT` / `EXCEPT` operator, if present.
    fn compound_op(&mut self) -> Option<CompoundOp> {
        if self.eat_kw("union") {
            if self.eat_kw("all") {
                Some(CompoundOp::UnionAll)
            } else {
                Some(CompoundOp::Union)
            }
        } else if self.eat_kw("intersect") {
            Some(CompoundOp::Intersect)
        } else if self.eat_kw("except") {
            Some(CompoundOp::Except)
        } else {
            None
        }
    }

    /// Parse a single query core: `SELECT … FROM … WHERE … GROUP BY … HAVING …`
    /// (no `WITH`, compound, `ORDER BY`, or `LIMIT` — those are handled by the
    /// enclosing [`select`](Self::select)).
    fn select_core(&mut self) -> Result<Select> {
        if self.check_kw("values") {
            return self.values_core();
        }
        self.expect_kw("select")?;
        let distinct = if self.eat_kw("distinct") {
            true
        } else {
            let _ = self.eat_kw("all");
            false
        };

        let mut columns = Vec::new();
        columns.push(self.result_column()?);
        while self.eat(&Token::Comma) {
            columns.push(self.result_column()?);
        }

        let from = if self.eat_kw("from") {
            Some(self.tables_clause()?)
        } else {
            None
        };
        let where_clause = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        let mut having = None;
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            group_by.push(self.expr()?);
            while self.eat(&Token::Comma) {
                group_by.push(self.expr()?);
            }
        }
        // HAVING may appear without GROUP BY (the whole result is one group),
        // matching SQLite.
        if self.eat_kw("having") {
            having = Some(self.expr()?);
        }

        // `WINDOW name AS (spec), …` named-window definitions.
        let mut window_defs = Vec::new();
        if self.eat_kw("window") {
            loop {
                let name = self.ident()?;
                self.expect_kw("as")?;
                let spec = self.window_paren_spec()?;
                window_defs.push((name, spec));
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }

        Ok(Select {
            ctes: Vec::new(),
            compound: Vec::new(),
            distinct,
            columns,
            from,
            where_clause,
            group_by,
            having,
            window_defs,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })
    }

    /// Parse a `VALUES (…), (…), …` query, desugared to a `SELECT` of the first
    /// row `UNION ALL`-ed with the rest. Columns are named `column1`, `column2`,
    /// … as SQLite does.
    fn values_core(&mut self) -> Result<Select> {
        self.expect_kw("values")?;
        let mut rows = Vec::new();
        rows.push(self.value_row()?);
        while self.eat(&Token::Comma) {
            rows.push(self.value_row()?);
        }
        let make = |exprs: Vec<Expr>| -> Select {
            let columns = exprs
                .into_iter()
                .enumerate()
                .map(|(i, e)| ResultColumn::Expr {
                    expr: e,
                    alias: Some(alloc::format!("column{}", i + 1)),
                })
                .collect();
            Select {
                ctes: Vec::new(),
                compound: Vec::new(),
                distinct: false,
                columns,
                from: None,
                where_clause: None,
                group_by: Vec::new(),
                having: None,
                window_defs: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
            }
        };
        let mut it = rows.into_iter();
        let mut core = make(it.next().expect("VALUES has at least one row"));
        for r in it {
            core.compound.push((CompoundOp::UnionAll, make(r)));
        }
        Ok(core)
    }

    fn result_column(&mut self) -> Result<ResultColumn> {
        if self.eat(&Token::Star) {
            return Ok(ResultColumn::Wildcard);
        }
        // `table.*` ?
        if let Some(Token::Word(_)) | Some(Token::Ident(_)) = self.peek() {
            let save = self.pos;
            let name = self.ident()?;
            if self.eat(&Token::Dot) {
                if self.eat(&Token::Star) {
                    return Ok(ResultColumn::TableWildcard(name));
                }
                self.pos = save; // not a table.* ; reparse as expression
            } else {
                self.pos = save;
            }
        }
        let expr = self.expr()?;
        let alias = self.opt_alias()?;
        Ok(ResultColumn::Expr { expr, alias })
    }

    fn opt_alias(&mut self) -> Result<Option<String>> {
        if self.eat_kw("as") {
            return Ok(Some(self.ident()?));
        }
        // A bare word that isn't a clause keyword can be an implicit alias.
        if let Some(Token::Word(w)) = self.peek() {
            if !is_reserved_after_expr(w) {
                return Ok(Some(self.ident()?));
            }
        } else if let Some(Token::Ident(_)) = self.peek() {
            return Ok(Some(self.ident()?));
        }
        Ok(None)
    }

    fn tables_clause(&mut self) -> Result<FromClause> {
        let first = self.table_ref()?;
        let mut joins = Vec::new();
        loop {
            if self.eat(&Token::Comma) {
                let table = self.table_ref()?;
                joins.push(Join {
                    kind: JoinKind::Inner,
                    table,
                    on: None,
                    natural: false,
                    using: Vec::new(),
                });
                continue;
            }
            // An optional `NATURAL` prefixes the join type.
            let natural = self.eat_kw("natural");
            let kind = if self.eat_kw("left") {
                let _ = self.eat_kw("outer");
                self.expect_kw("join")?;
                JoinKind::Left
            } else if self.eat_kw("right") {
                let _ = self.eat_kw("outer");
                self.expect_kw("join")?;
                JoinKind::Right
            } else if self.eat_kw("full") {
                let _ = self.eat_kw("outer");
                self.expect_kw("join")?;
                JoinKind::Full
            } else if self.eat_kw("inner") || self.eat_kw("cross") {
                self.expect_kw("join")?;
                JoinKind::Inner
            } else if self.eat_kw("join") {
                JoinKind::Inner
            } else if natural {
                return Err(self.err("expected JOIN after NATURAL"));
            } else {
                break;
            };
            let table = self.table_ref()?;
            // `ON <expr>` or `USING (col, …)` — at most one, and neither with
            // NATURAL.
            let mut on = None;
            let mut using = Vec::new();
            if self.eat_kw("on") {
                if natural {
                    return Err(self.err("NATURAL join may not have an ON clause"));
                }
                on = Some(self.expr()?);
            } else if self.eat_kw("using") {
                if natural {
                    return Err(self.err("NATURAL join may not have a USING clause"));
                }
                self.expect(&Token::LParen)?;
                using.push(self.ident()?);
                while self.eat(&Token::Comma) {
                    using.push(self.ident()?);
                }
                self.expect(&Token::RParen)?;
            }
            joins.push(Join {
                kind,
                table,
                on,
                natural,
                using,
            });
        }
        Ok(FromClause { first, joins })
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        // A derived table: `(SELECT …) [AS] alias`.
        if self.eat(&Token::LParen) {
            let select = self.select()?;
            self.expect(&Token::RParen)?;
            let alias = self.opt_alias()?;
            return Ok(TableRef {
                name: String::new(),
                schema: None,
                alias,
                subquery: Some(Box::new(select)),
                index_hint: None,
                tvf_args: None,
            });
        }
        // `[schema .] name` — a `.` qualifier names the database to resolve in.
        let mut name = self.ident()?;
        let mut schema = None;
        if self.eat(&Token::Dot) {
            schema = Some(name);
            name = self.ident()?;
        }
        // A table-valued function: `name(args)` as a FROM source.
        let tvf_args = if self.eat(&Token::LParen) {
            let mut args = Vec::new();
            if !self.check(&Token::RParen) {
                args.push(self.expr()?);
                while self.eat(&Token::Comma) {
                    args.push(self.expr()?);
                }
            }
            self.expect(&Token::RParen)?;
            Some(args)
        } else {
            None
        };
        let alias = self.opt_alias()?;
        let index_hint = self.index_hint()?;
        Ok(TableRef {
            name,
            schema,
            alias,
            subquery: None,
            index_hint,
            tvf_args,
        })
    }

    /// Parse an optional `INDEXED BY name` / `NOT INDEXED` hint after a table.
    fn index_hint(&mut self) -> Result<Option<IndexHint>> {
        if self.eat_kw("indexed") {
            self.expect_kw("by")?;
            return Ok(Some(IndexHint::IndexedBy(self.ident()?)));
        }
        if self.eat_kw("not") {
            self.expect_kw("indexed")?;
            return Ok(Some(IndexHint::NotIndexed));
        }
        Ok(None)
    }

    fn order_term(&mut self) -> Result<OrderTerm> {
        let expr = self.expr()?;
        let descending = if self.eat_kw("desc") {
            true
        } else {
            let _ = self.eat_kw("asc");
            false
        };
        let nulls_first = if self.eat_kw("nulls") {
            if self.eat_kw("first") {
                Some(true)
            } else {
                self.expect_kw("last")?;
                Some(false)
            }
        } else {
            None
        };
        Ok(OrderTerm {
            expr,
            descending,
            nulls_first,
        })
    }

    /// Parse `[schema .] name`, returning the optional schema qualifier and name.
    fn qualified_name(&mut self) -> Result<(Option<String>, String)> {
        let first = self.ident()?;
        if self.eat(&Token::Dot) {
            Ok((Some(first), self.ident()?))
        } else {
            Ok((None, first))
        }
    }

    fn insert(&mut self) -> Result<Insert> {
        // INSERT [OR <action>] INTO  /  REPLACE INTO
        let mut on_conflict = OnConflict::Abort;
        if self.eat_kw("insert") {
            if self.eat_kw("or") {
                on_conflict = if self.eat_kw("replace") {
                    OnConflict::Replace
                } else if self.eat_kw("ignore") {
                    OnConflict::Ignore
                } else {
                    let _ = self.advance(); // ABORT / ROLLBACK / FAIL
                    OnConflict::Abort
                };
            }
        } else {
            self.expect_kw("replace")?;
            on_conflict = OnConflict::Replace;
        }
        self.expect_kw("into")?;
        let (schema, table) = self.qualified_name()?;
        let mut columns = Vec::new();
        if self.eat(&Token::LParen) {
            columns.push(self.ident()?);
            while self.eat(&Token::Comma) {
                columns.push(self.ident()?);
            }
            self.expect(&Token::RParen)?;
        }

        let source = if self.eat_kw("default") {
            self.expect_kw("values")?;
            InsertSource::DefaultValues
        } else if self.check_kw("select") || self.check_kw("with") {
            InsertSource::Select(Box::new(self.select()?))
        } else {
            self.expect_kw("values")?;
            let mut rows = Vec::new();
            rows.push(self.value_row()?);
            while self.eat(&Token::Comma) {
                rows.push(self.value_row()?);
            }
            InsertSource::Values(rows)
        };
        let upsert = self.upsert_clause()?;
        let returning = self.returning_clause()?;
        Ok(Insert {
            table,
            schema,
            columns,
            source,
            on_conflict,
            upsert,
            returning,
        })
    }

    /// Parse an optional `ON CONFLICT [(target) [WHERE …]] DO {NOTHING | UPDATE …}`.
    fn upsert_clause(&mut self) -> Result<Option<Upsert>> {
        if !self.eat_kw("on") {
            return Ok(None);
        }
        self.expect_kw("conflict")?;
        let mut target = Vec::new();
        let mut target_where = None;
        if self.eat(&Token::LParen) {
            target.push(self.ident()?);
            while self.eat(&Token::Comma) {
                target.push(self.ident()?);
            }
            self.expect(&Token::RParen)?;
            if self.eat_kw("where") {
                target_where = Some(self.expr()?);
            }
        }
        self.expect_kw("do")?;
        let action = if self.eat_kw("nothing") {
            UpsertAction::Nothing
        } else {
            self.expect_kw("update")?;
            self.expect_kw("set")?;
            let mut assignments = Vec::new();
            loop {
                let col = self.ident()?;
                self.expect(&Token::Eq)?;
                let value = self.expr()?;
                assignments.push((col, value));
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            let where_clause = if self.eat_kw("where") {
                Some(self.expr()?)
            } else {
                None
            };
            UpsertAction::Update {
                assignments,
                where_clause,
            }
        };
        Ok(Some(Upsert {
            target,
            target_where,
            action,
        }))
    }

    /// Parse an optional `RETURNING <result columns>`; empty when absent.
    fn returning_clause(&mut self) -> Result<Vec<ResultColumn>> {
        if !self.eat_kw("returning") {
            return Ok(Vec::new());
        }
        let mut cols = Vec::new();
        cols.push(self.result_column()?);
        while self.eat(&Token::Comma) {
            cols.push(self.result_column()?);
        }
        Ok(cols)
    }

    fn value_row(&mut self) -> Result<Vec<Expr>> {
        self.expect(&Token::LParen)?;
        let mut row = Vec::new();
        row.push(self.expr()?);
        while self.eat(&Token::Comma) {
            row.push(self.expr()?);
        }
        self.expect(&Token::RParen)?;
        Ok(row)
    }

    fn update(&mut self) -> Result<Update> {
        self.expect_kw("update")?;
        // `UPDATE OR <action>` conflict clause (REPLACE/IGNORE keep their own
        // resolution; ROLLBACK/ABORT/FAIL all abort the statement, like INSERT).
        let on_conflict = if self.eat_kw("or") {
            if self.eat_kw("replace") {
                OnConflict::Replace
            } else if self.eat_kw("ignore") {
                OnConflict::Ignore
            } else if self.eat_kw("rollback") || self.eat_kw("abort") || self.eat_kw("fail") {
                OnConflict::Abort
            } else {
                return Err(self.err("expected REPLACE/IGNORE/ROLLBACK/ABORT/FAIL after UPDATE OR"));
            }
        } else {
            OnConflict::Abort
        };
        let (schema, table) = self.qualified_name()?;
        self.expect_kw("set")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            self.expect(&Token::Eq)?;
            let value = self.expr()?;
            assignments.push((col, value));
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        // `FROM <sources>` (SQLite's UPDATE-FROM extension) sits between SET and
        // WHERE.
        let from = if self.eat_kw("from") {
            Some(self.tables_clause()?)
        } else {
            None
        };
        let where_clause = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        // RETURNING comes before any ORDER BY/LIMIT extension.
        let returning = self.returning_clause()?;
        let (order_by, limit, offset) = self.order_limit_offset()?;
        Ok(Update {
            table,
            schema,
            on_conflict,
            assignments,
            from,
            where_clause,
            order_by,
            limit,
            offset,
            returning,
        })
    }

    fn delete(&mut self) -> Result<Delete> {
        self.expect_kw("delete")?;
        self.expect_kw("from")?;
        let (schema, table) = self.qualified_name()?;
        let where_clause = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.returning_clause()?;
        let (order_by, limit, offset) = self.order_limit_offset()?;
        Ok(Delete {
            table,
            schema,
            where_clause,
            order_by,
            limit,
            offset,
            returning,
        })
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_kw("create")?;
        let unique = self.eat_kw("unique");
        // `TEMP`/`TEMPORARY` before TABLE/VIEW/TRIGGER routes the object to the
        // `temp` database (modeled as a `schema = "temp"` qualifier).
        let temp = self.eat_kw("temp") || self.eat_kw("temporary");
        if self.eat_kw("table") {
            if unique {
                return Err(self.err("UNIQUE is not valid for CREATE TABLE"));
            }
            let mut ct = self.create_table()?;
            if temp && ct.schema.is_none() {
                ct.schema = Some("temp".into());
            }
            return Ok(Statement::CreateTable(ct));
        }
        if self.eat_kw("index") {
            let mut ci = self.create_index(unique)?;
            if temp && ci.schema.is_none() {
                ci.schema = Some("temp".into());
            }
            return Ok(Statement::CreateIndex(ci));
        }
        if unique {
            return Err(self.err("expected INDEX after CREATE UNIQUE"));
        }
        if self.eat_kw("view") {
            let mut cv = self.create_view()?;
            if temp && cv.schema.is_none() {
                cv.schema = Some("temp".into());
            }
            return Ok(Statement::CreateView(cv));
        }
        if self.eat_kw("trigger") {
            let mut ct = self.create_trigger()?;
            if temp && ct.schema.is_none() {
                ct.schema = Some("temp".into());
            }
            return Ok(Statement::CreateTrigger(ct));
        }
        if self.eat_kw("virtual") {
            self.expect_kw("table")?;
            let mut cvt = self.create_virtual_table()?;
            if temp && cvt.schema.is_none() {
                cvt.schema = Some("temp".into());
            }
            return Ok(Statement::CreateVirtualTable(cvt));
        }
        Err(self.err("expected TABLE, INDEX, VIEW, TRIGGER, or VIRTUAL TABLE after CREATE"))
    }

    fn create_trigger(&mut self) -> Result<CreateTrigger> {
        let if_not_exists = self.if_not_exists()?;
        let (schema, name) = self.qualified_name()?;
        let timing = if self.eat_kw("before") {
            TriggerTiming::Before
        } else if self.eat_kw("after") {
            TriggerTiming::After
        } else if self.eat_kw("instead") {
            self.expect_kw("of")?;
            TriggerTiming::InsteadOf
        } else {
            TriggerTiming::Before // SQLite's default
        };
        let event = if self.eat_kw("insert") {
            TriggerEvent::Insert
        } else if self.eat_kw("delete") {
            TriggerEvent::Delete
        } else if self.eat_kw("update") {
            let mut cols = Vec::new();
            if self.eat_kw("of") {
                cols.push(self.ident()?);
                while self.eat(&Token::Comma) {
                    cols.push(self.ident()?);
                }
            }
            TriggerEvent::Update(cols)
        } else {
            return Err(self.err("expected INSERT, UPDATE, or DELETE in CREATE TRIGGER"));
        };
        self.expect_kw("on")?;
        let table = self.ident()?;
        if self.eat_kw("for") {
            self.expect_kw("each")?;
            self.expect_kw("row")?;
        }
        let when = if self.eat_kw("when") {
            Some(self.expr()?)
        } else {
            None
        };
        self.expect_kw("begin")?;
        let mut body = Vec::new();
        while !self.check_kw("end") && !self.at_end() {
            let stmt = self.statement()?;
            body.push(stmt);
            // Each body statement is terminated by a semicolon.
            let _ = self.eat(&Token::Semicolon);
        }
        self.expect_kw("end")?;
        Ok(CreateTrigger {
            if_not_exists,
            schema,
            name,
            timing,
            event,
            table,
            when,
            body,
        })
    }

    fn create_view(&mut self) -> Result<CreateView> {
        let if_not_exists = self.if_not_exists()?;
        let (schema, name) = self.qualified_name()?;
        let mut columns = Vec::new();
        if self.eat(&Token::LParen) {
            columns.push(self.ident()?);
            while self.eat(&Token::Comma) {
                columns.push(self.ident()?);
            }
            self.expect(&Token::RParen)?;
        }
        self.expect_kw("as")?;
        let select = Box::new(self.select()?);
        Ok(CreateView {
            if_not_exists,
            schema,
            name,
            columns,
            select,
        })
    }

    fn create_virtual_table(&mut self) -> Result<CreateVirtualTable> {
        let if_not_exists = self.if_not_exists()?;
        let (schema, name) = self.qualified_name()?;
        self.expect_kw("using")?;
        let module = self.ident()?;
        let mut args = Vec::new();
        if self.eat(&Token::LParen) {
            // SQLite passes the module arguments to the module verbatim; we don't
            // evaluate them as expressions. Capture each comma-separated argument
            // as a string, reassembling the raw token text. A bare comma at depth
            // zero separates arguments; nested parentheses are kept within an arg.
            if !self.check(&Token::RParen) {
                loop {
                    args.push(self.vtab_arg()?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Token::RParen)?;
        }
        Ok(CreateVirtualTable {
            if_not_exists,
            schema,
            name,
            module,
            args,
        })
    }

    /// Capture one module argument of a `CREATE VIRTUAL TABLE … USING m(…)` list
    /// verbatim as a string, stopping at a top-level comma or the closing paren.
    fn vtab_arg(&mut self) -> Result<String> {
        let mut out = String::new();
        let mut depth = 0usize;
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated virtual-table argument list")),
                Some(Token::RParen) if depth == 0 => break,
                Some(Token::Comma) if depth == 0 => break,
                Some(_) => {}
            }
            let tok = self.advance().expect("peeked");
            match &tok {
                Token::LParen => depth += 1,
                Token::RParen => depth = depth.saturating_sub(1),
                _ => {}
            }
            let text = token_arg_text(&tok);
            // Separate two word-like tokens with a space, but keep a sign or dot
            // adjacent to its number (so `- 5` stays `-5`).
            let needs_space = !out.is_empty()
                && ends_wordish(&out)
                && text.chars().next().is_some_and(is_wordish_start);
            if needs_space {
                out.push(' ');
            }
            out.push_str(&text);
        }
        if out.is_empty() {
            return Err(self.err("empty virtual-table argument"));
        }
        Ok(out)
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn create_table(&mut self) -> Result<CreateTable> {
        let if_not_exists = self.if_not_exists()?;
        let (schema, name) = self.qualified_name()?;
        // `CREATE TABLE name AS SELECT …`.
        if self.eat_kw("as") {
            let select = self.select()?;
            return Ok(CreateTable {
                if_not_exists,
                name,
                schema,
                columns: Vec::new(),
                constraints: Vec::new(),
                without_rowid: false,
                strict: false,
                as_select: Some(Box::new(select)),
            });
        }
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.starts_table_constraint() {
                if let Some(tc) = self.table_constraint()? {
                    constraints.push(tc);
                }
            } else {
                columns.push(self.column_def()?);
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RParen)?;
        // Table options after the column list: `WITHOUT ROWID` and/or `STRICT`,
        // comma-separated in any order.
        let mut without_rowid = false;
        let mut strict = false;
        loop {
            if self.eat_kw("without") {
                self.expect_kw("rowid")?;
                without_rowid = true;
            } else if self.eat_kw("strict") {
                strict = true;
            } else {
                break;
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        Ok(CreateTable {
            if_not_exists,
            name,
            schema,
            columns,
            constraints,
            without_rowid,
            strict,
            as_select: None,
        })
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.ident()?;
        // Optional type name: one or more bare words, optionally with (n[,m]).
        let mut type_name = None;
        if let Some(Token::Word(_)) = self.peek() {
            if !is_column_constraint_kw(self.peek()) {
                let mut t = self.ident()?;
                while let Some(Token::Word(_)) = self.peek() {
                    if is_column_constraint_kw(self.peek()) {
                        break;
                    }
                    t.push(' ');
                    t.push_str(&self.ident()?);
                }
                if self.eat(&Token::LParen) {
                    // length/precision args; consume to matching paren
                    while !self.check(&Token::RParen) && !self.at_end() {
                        self.advance();
                    }
                    self.expect(&Token::RParen)?;
                }
                type_name = Some(t);
            }
        }
        let mut constraints = Vec::new();
        loop {
            if self.eat_kw("constraint") {
                let _ = self.ident()?; // named constraint; the constraint follows
            } else if self.eat_kw("primary") {
                self.expect_kw("key")?;
                let descending = if self.eat_kw("desc") {
                    true
                } else {
                    let _ = self.eat_kw("asc");
                    false
                };
                self.eat_conflict_clause();
                let autoincrement = self.eat_kw("autoincrement");
                constraints.push(ColumnConstraint::PrimaryKey {
                    descending,
                    autoincrement,
                });
            } else if self.eat_kw("not") {
                self.expect_kw("null")?;
                self.eat_conflict_clause();
                constraints.push(ColumnConstraint::NotNull);
            } else if self.eat_kw("null") {
                // A bare NULL (explicitly nullable): no constraint to record.
            } else if self.eat_kw("unique") {
                self.eat_conflict_clause();
                constraints.push(ColumnConstraint::Unique);
            } else if self.eat_kw("default") {
                let e = if self.check(&Token::LParen) {
                    self.expect(&Token::LParen)?;
                    let e = self.expr()?;
                    self.expect(&Token::RParen)?;
                    e
                } else {
                    self.expr()?
                };
                constraints.push(ColumnConstraint::Default(e));
            } else if self.eat_kw("collate") {
                constraints.push(ColumnConstraint::Collate(self.ident()?));
            } else if self.eat_kw("check") {
                self.expect(&Token::LParen)?;
                let e = self.expr()?;
                self.expect(&Token::RParen)?;
                constraints.push(ColumnConstraint::Check(e));
            } else if self.eat_kw("references") {
                let fk = self.parse_fk_clause(alloc::vec![name.clone()])?;
                constraints.push(ColumnConstraint::References(fk));
            } else if self.eat_kw("generated") {
                let _ = self.eat_kw("always");
                self.expect_kw("as")?;
                constraints.push(self.generated_column()?);
            } else if self.eat_kw("as") {
                constraints.push(self.generated_column()?);
            } else {
                break;
            }
        }
        Ok(ColumnDef {
            name,
            type_name,
            constraints,
        })
    }

    /// Parse the tail of a generated-column clause, after `AS`:
    /// `(expr) [STORED|VIRTUAL]`.
    fn generated_column(&mut self) -> Result<ColumnConstraint> {
        self.expect(&Token::LParen)?;
        let expr = self.expr()?;
        self.expect(&Token::RParen)?;
        let stored = if self.eat_kw("stored") {
            true
        } else {
            let _ = self.eat_kw("virtual");
            false
        };
        Ok(ColumnConstraint::Generated { expr, stored })
    }

    /// Whether the next item in a `CREATE TABLE` body is a table constraint
    /// (rather than a column definition).
    fn starts_table_constraint(&self) -> bool {
        self.check_kw("constraint")
            || self.check_kw("primary")
            || self.check_kw("unique")
            || self.check_kw("check")
            || self.check_kw("foreign")
    }

    /// Parse one table constraint, returning `None` for kinds we accept but do
    /// not yet model (`CHECK`, `FOREIGN KEY`).
    fn table_constraint(&mut self) -> Result<Option<TableConstraint>> {
        if self.eat_kw("constraint") {
            let _ = self.ident()?; // named constraint
        }
        if self.eat_kw("primary") {
            self.expect_kw("key")?;
            let cols = self.paren_columns()?;
            self.eat_conflict_clause();
            Ok(Some(TableConstraint::PrimaryKey(cols)))
        } else if self.eat_kw("unique") {
            let cols = self.paren_columns()?;
            self.eat_conflict_clause();
            Ok(Some(TableConstraint::Unique(cols)))
        } else if self.eat_kw("check") {
            self.expect(&Token::LParen)?;
            let e = self.expr()?;
            self.expect(&Token::RParen)?;
            Ok(Some(TableConstraint::Check(e)))
        } else if self.eat_kw("foreign") {
            self.expect_kw("key")?;
            let columns = self.paren_columns()?;
            self.expect_kw("references")?;
            let fk = self.parse_fk_clause(columns)?;
            Ok(Some(TableConstraint::ForeignKey(fk)))
        } else {
            Err(self.err("expected a table constraint"))
        }
    }

    /// Consume an `ON CONFLICT <action>` clause if present.
    fn eat_conflict_clause(&mut self) {
        if self.eat_kw("on") {
            let _ = self.eat_kw("conflict");
            let _ = self.advance(); // the action keyword
        }
    }

    /// Parse the tail of a `REFERENCES` clause (target table, optional parent
    /// columns, and `ON DELETE/UPDATE …` / `MATCH …` / `DEFERRABLE …` actions)
    /// into a [`ForeignKey`], given the child `columns`.
    fn parse_fk_clause(&mut self, columns: Vec<String>) -> Result<ForeignKey> {
        let ref_table = self.ident()?;
        let ref_columns = if self.check(&Token::LParen) {
            self.paren_columns()?
        } else {
            Vec::new()
        };
        let mut on_delete = FkAction::default();
        let mut on_update = FkAction::default();
        loop {
            if self.eat_kw("on") {
                let is_delete = self.eat_kw("delete");
                if !is_delete {
                    let _ = self.eat_kw("update");
                }
                let action = if self.eat_kw("set") {
                    if self.eat_kw("null") {
                        FkAction::SetNull
                    } else {
                        let _ = self.eat_kw("default");
                        FkAction::SetDefault
                    }
                } else if self.eat_kw("cascade") {
                    FkAction::Cascade
                } else if self.eat_kw("restrict") {
                    FkAction::Restrict
                } else if self.eat_kw("no") {
                    let _ = self.eat_kw("action");
                    FkAction::NoAction
                } else {
                    FkAction::NoAction
                };
                if is_delete {
                    on_delete = action;
                } else {
                    on_update = action;
                }
            } else if self.eat_kw("match") {
                let _ = self.advance();
            } else if self.eat_kw("not") {
                let _ = self.eat_kw("deferrable");
                if self.eat_kw("initially") {
                    let _ = self.advance();
                }
            } else if self.eat_kw("deferrable") {
                if self.eat_kw("initially") {
                    let _ = self.advance();
                }
            } else {
                break;
            }
        }
        Ok(ForeignKey {
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        })
    }

    /// A parenthesized column list, tolerating per-column `COLLATE`/`ASC`/`DESC`.
    fn paren_columns(&mut self) -> Result<Vec<String>> {
        self.expect(&Token::LParen)?;
        let mut names = Vec::new();
        loop {
            names.push(self.ident()?);
            if self.eat_kw("collate") {
                let _ = self.ident()?;
            }
            let _ = self.eat_kw("asc") || self.eat_kw("desc");
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RParen)?;
        Ok(names)
    }

    fn create_index(&mut self, unique: bool) -> Result<CreateIndex> {
        let if_not_exists = self.if_not_exists()?;
        let (schema, name) = self.qualified_name()?;
        self.expect_kw("on")?;
        let table = self.ident()?;
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        columns.push(self.order_term()?);
        while self.eat(&Token::Comma) {
            columns.push(self.order_term()?);
        }
        self.expect(&Token::RParen)?;
        let where_clause = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(CreateIndex {
            unique,
            if_not_exists,
            schema,
            name,
            table,
            columns,
            where_clause,
        })
    }

    fn drop_stmt(&mut self) -> Result<Drop> {
        self.expect_kw("drop")?;
        let kind = if self.eat_kw("table") {
            DropKind::Table
        } else if self.eat_kw("index") {
            DropKind::Index
        } else if self.eat_kw("view") {
            DropKind::View
        } else if self.eat_kw("trigger") {
            DropKind::Trigger
        } else {
            return Err(self.err("expected TABLE/INDEX/VIEW/TRIGGER after DROP"));
        };
        let if_exists = if self.eat_kw("if") {
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let (schema, name) = self.qualified_name()?;
        Ok(Drop {
            kind,
            if_exists,
            name,
            schema,
        })
    }

    fn alter(&mut self) -> Result<Alter> {
        self.expect_kw("alter")?;
        self.expect_kw("table")?;
        let (schema, table) = self.qualified_name()?;
        let action = if self.eat_kw("rename") {
            if self.eat_kw("to") {
                AlterAction::RenameTable(self.ident()?)
            } else {
                let _ = self.eat_kw("column");
                let old = self.ident()?;
                self.expect_kw("to")?;
                let new = self.ident()?;
                AlterAction::RenameColumn { old, new }
            }
        } else if self.eat_kw("add") {
            let _ = self.eat_kw("column");
            AlterAction::AddColumn(self.column_def()?)
        } else if self.eat_kw("drop") {
            let _ = self.eat_kw("column");
            AlterAction::DropColumn(self.ident()?)
        } else {
            return Err(self.err("expected RENAME, ADD, or DROP after ALTER TABLE"));
        };
        Ok(Alter {
            schema,
            table,
            action,
        })
    }

    // ---- expressions (Pratt) ------------------------------------------------

    fn expr(&mut self) -> Result<Expr> {
        self.expr_bp(0)
    }

    fn expr_bp(&mut self, min_bp: u8) -> Result<Expr> {
        let _guard = self.enter()?;
        let mut left = self.prefix()?;
        while let Some((op, bp)) = self.peek_infix() {
            if bp < min_bp {
                break;
            }
            left = self.infix(left, op, bp)?;
        }
        Ok(left)
    }

    fn prefix(&mut self) -> Result<Expr> {
        match self.peek() {
            Some(Token::Minus) => {
                self.pos += 1;
                // `-9223372036854775808` (possibly after whitespace) is exactly
                // i64::MIN — fold it into an integer literal like SQLite, rather
                // than negating the real that `2^63` would otherwise produce.
                if matches!(self.peek(), Some(Token::Int2Pow63)) {
                    self.pos += 1;
                    return Ok(Expr::Literal(Literal::Integer(i64::MIN)));
                }
                Ok(Expr::Unary {
                    op: UnaryOp::Negate,
                    expr: Box::new(self.expr_bp(BP_UNARY)?),
                })
            }
            Some(Token::Plus) => {
                self.pos += 1;
                Ok(Expr::Unary {
                    op: UnaryOp::Identity,
                    expr: Box::new(self.expr_bp(BP_UNARY)?),
                })
            }
            Some(Token::BitNot) => {
                self.pos += 1;
                Ok(Expr::Unary {
                    op: UnaryOp::BitNot,
                    expr: Box::new(self.expr_bp(BP_UNARY)?),
                })
            }
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("not") => {
                self.pos += 1;
                Ok(Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(self.expr_bp(BP_NOT_PREFIX)?),
                })
            }
            _ => self.primary_collate(),
        }
    }

    /// A primary expression, with any trailing `COLLATE name` postfix applied
    /// (COLLATE binds tighter than every operator).
    fn primary_collate(&mut self) -> Result<Expr> {
        let mut e = self.primary()?;
        while self.eat_kw("collate") {
            let collation = self.ident()?;
            e = Expr::Collate {
                expr: Box::new(e),
                collation,
            };
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.advance() {
            Some(Token::Integer(i)) => Ok(Expr::Literal(Literal::Integer(i))),
            // `2^63` used positively is a real, like any i64-overflowing integer.
            Some(Token::Int2Pow63) => Ok(Expr::Literal(Literal::Real(9223372036854775808.0))),
            Some(Token::Float(f)) => Ok(Expr::Literal(Literal::Real(f))),
            Some(Token::Str(s)) => Ok(Expr::Literal(Literal::Str(s))),
            Some(Token::Blob(b)) => Ok(Expr::Literal(Literal::Blob(b))),
            Some(Token::Param(p)) => Ok(Expr::Parameter(p)),
            Some(Token::LParen) => {
                if self.check_kw("select") || self.check_kw("with") {
                    let sel = self.select()?;
                    self.expect(&Token::RParen)?;
                    Ok(Expr::Subquery(Box::new(sel)))
                } else {
                    let first = self.expr()?;
                    if self.eat(&Token::Comma) {
                        // A row value `(a, b, …)`.
                        let mut items = alloc::vec![first];
                        items.push(self.expr()?);
                        while self.eat(&Token::Comma) {
                            items.push(self.expr()?);
                        }
                        self.expect(&Token::RParen)?;
                        Ok(Expr::RowValue(items))
                    } else {
                        self.expect(&Token::RParen)?;
                        Ok(Expr::Paren(Box::new(first)))
                    }
                }
            }
            Some(Token::Ident(name)) => self.after_name(name, true),
            Some(Token::Word(w)) => {
                let lw = w.to_ascii_lowercase();
                match lw.as_str() {
                    "null" => Ok(Expr::Literal(Literal::Null)),
                    "true" => Ok(Expr::Literal(Literal::Boolean(true))),
                    "false" => Ok(Expr::Literal(Literal::Boolean(false))),
                    // SQL date/time keywords (UTC), equivalent to the
                    // `date`/`time`/`datetime` functions on `'now'`.
                    "current_date" => Ok(now_datetime_fn("date")),
                    "current_time" => Ok(now_datetime_fn("time")),
                    "current_timestamp" => Ok(now_datetime_fn("datetime")),
                    "case" => self.case_expr(),
                    "cast" => self.cast_expr(),
                    "exists" => {
                        self.expect(&Token::LParen)?;
                        let sel = self.select()?;
                        self.expect(&Token::RParen)?;
                        Ok(Expr::Exists {
                            select: Box::new(sel),
                            negated: false,
                        })
                    }
                    _ if is_reserved_keyword(&lw) => Err(Error::Parse(format!(
                        "unexpected keyword {w:?} in expression"
                    ))),
                    _ => self.after_name(w, false),
                }
            }
            other => Err(Error::Parse(format!(
                "expected an expression, found {other:?}"
            ))),
        }
    }

    /// Continue parsing after a name: `name`, `tbl.col`, or `func(args)`.
    fn after_name(&mut self, name: String, quoted: bool) -> Result<Expr> {
        if !quoted && self.eat(&Token::LParen) {
            return self.function_call(name);
        }
        if self.eat(&Token::Dot) {
            let mut table = name;
            let mut column = self.ident()?;
            // A third dotted part means `schema.table.column`: the database
            // qualifier is redundant for name resolution (a table/alias name is
            // unique within a query's scope), so keep the table + column.
            if self.eat(&Token::Dot) {
                table = column;
                column = self.ident()?;
            }
            return Ok(Expr::Column {
                table: Some(table),
                column,
            });
        }
        Ok(Expr::Column {
            table: None,
            column: name,
        })
    }

    fn function_call(&mut self, name: String) -> Result<Expr> {
        if self.eat(&Token::Star) {
            self.expect(&Token::RParen)?;
            let filter = self.filter_clause()?;
            let over = self.window_over()?;
            return Ok(Expr::Function {
                name,
                distinct: false,
                args: Vec::new(),
                star: true,
                filter,
                order_by: Vec::new(),
                over,
            });
        }
        let distinct = self.eat_kw("distinct");
        let mut args = Vec::new();
        if !self.check(&Token::RParen) {
            args.push(self.expr()?);
            while self.eat(&Token::Comma) {
                args.push(self.expr()?);
            }
        }
        // `ORDER BY …` inside an aggregate call (e.g. `group_concat(x ORDER BY y)`).
        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            order_by.push(self.order_term()?);
            while self.eat(&Token::Comma) {
                order_by.push(self.order_term()?);
            }
        }
        self.expect(&Token::RParen)?;
        let filter = self.filter_clause()?;
        let over = self.window_over()?;
        Ok(Expr::Function {
            name,
            distinct,
            args,
            star: false,
            filter,
            order_by,
            over,
        })
    }

    /// Parse an optional `FILTER (WHERE <expr>)` aggregate/window filter.
    fn filter_clause(&mut self) -> Result<Option<Box<Expr>>> {
        if !self.eat_kw("filter") {
            return Ok(None);
        }
        self.expect(&Token::LParen)?;
        self.expect_kw("where")?;
        let e = self.expr()?;
        self.expect(&Token::RParen)?;
        Ok(Some(Box::new(e)))
    }

    /// Parse an optional `OVER ( [PARTITION BY …] [ORDER BY …] )` clause.
    fn window_over(&mut self) -> Result<Option<WindowSpec>> {
        if !self.eat_kw("over") {
            return Ok(None);
        }
        // `OVER window_name` references a named window; `OVER (…)` is inline.
        if !self.check(&Token::LParen) {
            let name = self.ident()?;
            return Ok(Some(WindowSpec {
                base_name: Some(name),
                ..WindowSpec::default()
            }));
        }
        Ok(Some(self.window_paren_spec()?))
    }

    /// Parse a parenthesized window spec `( [name] [PARTITION BY …] [ORDER BY …]
    /// [frame] )`. A leading bare name inherits a named window's spec.
    fn window_paren_spec(&mut self) -> Result<WindowSpec> {
        self.expect(&Token::LParen)?;
        let mut spec = WindowSpec::default();
        // An optional base window name (anything that isn't a clause keyword).
        if let Some(Token::Word(w)) = self.peek() {
            let lw = w.to_ascii_lowercase();
            if !matches!(
                lw.as_str(),
                "partition" | "order" | "rows" | "range" | "groups"
            ) {
                spec.base_name = Some(self.ident()?);
            }
        }
        if self.eat_kw("partition") {
            self.expect_kw("by")?;
            spec.partition_by.push(self.expr()?);
            while self.eat(&Token::Comma) {
                spec.partition_by.push(self.expr()?);
            }
        }
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            spec.order_by.push(self.order_term()?);
            while self.eat(&Token::Comma) {
                spec.order_by.push(self.order_term()?);
            }
        }
        spec.frame = self.window_frame()?;
        self.expect(&Token::RParen)?;
        Ok(spec)
    }

    /// Parse an optional frame clause: `(ROWS|RANGE|GROUPS) (BETWEEN a AND b | a)`.
    fn window_frame(&mut self) -> Result<Option<WindowFrame>> {
        let mode = if self.eat_kw("rows") {
            FrameMode::Rows
        } else if self.eat_kw("range") {
            FrameMode::Range
        } else if self.eat_kw("groups") {
            FrameMode::Groups
        } else {
            return Ok(None);
        };
        let (start, end) = if self.eat_kw("between") {
            let s = self.frame_bound()?;
            self.expect_kw("and")?;
            let e = self.frame_bound()?;
            (s, e)
        } else {
            // A bare start bound; the end defaults to CURRENT ROW.
            (self.frame_bound()?, FrameBound::CurrentRow)
        };
        // Optional EXCLUDE clause.
        let exclude = if self.eat_kw("exclude") {
            if self.eat_kw("no") {
                self.expect_kw("others")?;
                FrameExclude::NoOthers
            } else if self.eat_kw("current") {
                self.expect_kw("row")?;
                FrameExclude::CurrentRow
            } else if self.eat_kw("group") {
                FrameExclude::Group
            } else {
                self.expect_kw("ties")?;
                FrameExclude::Ties
            }
        } else {
            FrameExclude::NoOthers
        };
        Ok(Some(WindowFrame {
            mode,
            start,
            end,
            exclude,
        }))
    }

    fn frame_bound(&mut self) -> Result<FrameBound> {
        if self.eat_kw("unbounded") {
            if self.eat_kw("preceding") {
                return Ok(FrameBound::UnboundedPreceding);
            }
            self.expect_kw("following")?;
            return Ok(FrameBound::UnboundedFollowing);
        }
        if self.eat_kw("current") {
            self.expect_kw("row")?;
            return Ok(FrameBound::CurrentRow);
        }
        // `<n> PRECEDING|FOLLOWING`
        let n = match self.expr()? {
            Expr::Literal(Literal::Integer(i)) => i,
            _ => return Err(self.err("expected an integer frame offset")),
        };
        if self.eat_kw("preceding") {
            Ok(FrameBound::Preceding(n))
        } else if self.eat_kw("following") {
            Ok(FrameBound::Following(n))
        } else {
            Err(self.err("expected PRECEDING or FOLLOWING"))
        }
    }

    fn case_expr(&mut self) -> Result<Expr> {
        let operand = if !self.check_kw("when") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        let mut when_then = Vec::new();
        while self.eat_kw("when") {
            let cond = self.expr()?;
            self.expect_kw("then")?;
            let result = self.expr()?;
            when_then.push((cond, result));
        }
        if when_then.is_empty() {
            return Err(self.err("CASE requires at least one WHEN"));
        }
        let else_result = if self.eat_kw("else") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("end")?;
        Ok(Expr::Case {
            operand,
            when_then,
            else_result,
        })
    }

    fn cast_expr(&mut self) -> Result<Expr> {
        self.expect(&Token::LParen)?;
        let expr = Box::new(self.expr()?);
        self.expect_kw("as")?;
        // A type name is one or more bare words, optionally followed by a size
        // suffix `(n[,m])` (e.g. `VARCHAR(10)`, `DECIMAL(10,2)`). Only the words
        // matter for affinity; the size is parsed and ignored.
        let mut type_name = self.ident()?;
        while let Some(Token::Word(_)) = self.peek() {
            type_name.push(' ');
            type_name.push_str(&self.ident()?);
        }
        if self.eat(&Token::LParen) {
            while !self.check(&Token::RParen) && !self.at_end() {
                self.advance();
            }
            self.expect(&Token::RParen)?;
        }
        self.expect(&Token::RParen)?;
        Ok(Expr::Cast { expr, type_name })
    }

    /// Peek at the current infix operator and its binding power, if any.
    fn peek_infix(&self) -> Option<(InfixOp, u8)> {
        let tok = self.peek()?;
        let op = match tok {
            Token::Concat => (InfixOp::Binary(BinaryOp::Concat), BP_CONCAT),
            Token::Star => (InfixOp::Binary(BinaryOp::Mul), BP_MUL),
            Token::Slash => (InfixOp::Binary(BinaryOp::Div), BP_MUL),
            Token::Percent => (InfixOp::Binary(BinaryOp::Mod), BP_MUL),
            Token::Plus => (InfixOp::Binary(BinaryOp::Add), BP_ADD),
            Token::Minus => (InfixOp::Binary(BinaryOp::Sub), BP_ADD),
            Token::BitAnd => (InfixOp::Binary(BinaryOp::BitAnd), BP_BIT),
            Token::BitOr => (InfixOp::Binary(BinaryOp::BitOr), BP_BIT),
            Token::LShift => (InfixOp::Binary(BinaryOp::LShift), BP_BIT),
            Token::RShift => (InfixOp::Binary(BinaryOp::RShift), BP_BIT),
            Token::Arrow => (InfixOp::Binary(BinaryOp::JsonExtract), BP_CONCAT),
            Token::Arrow2 => (InfixOp::Binary(BinaryOp::JsonExtractText), BP_CONCAT),
            Token::Lt => (InfixOp::Binary(BinaryOp::Lt), BP_REL),
            Token::LtEq => (InfixOp::Binary(BinaryOp::LtEq), BP_REL),
            Token::Gt => (InfixOp::Binary(BinaryOp::Gt), BP_REL),
            Token::GtEq => (InfixOp::Binary(BinaryOp::GtEq), BP_REL),
            Token::Eq => (InfixOp::Binary(BinaryOp::Eq), BP_EQ),
            Token::NotEq => (InfixOp::Binary(BinaryOp::NotEq), BP_EQ),
            Token::Word(w) => {
                let lw = w.to_ascii_lowercase();
                match lw.as_str() {
                    "or" => (InfixOp::Binary(BinaryOp::Or), BP_OR),
                    "and" => (InfixOp::Binary(BinaryOp::And), BP_AND),
                    "is" => (InfixOp::Is, BP_EQ),
                    "in" => (InfixOp::In { negated: false }, BP_EQ),
                    "like" => (InfixOp::Like { negated: false }, BP_EQ),
                    "glob" => (InfixOp::Binary(BinaryOp::Glob), BP_EQ),
                    "between" => (InfixOp::Between { negated: false }, BP_EQ),
                    "not" => (InfixOp::NotPrefixed, BP_EQ),
                    "isnull" => (InfixOp::IsNullKw { negated: false }, BP_EQ),
                    "notnull" => (InfixOp::IsNullKw { negated: true }, BP_EQ),
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(op)
    }

    fn infix(&mut self, left: Expr, op: InfixOp, bp: u8) -> Result<Expr> {
        match op {
            InfixOp::Binary(b) => {
                self.pos += 1;
                // Left-associative: right side binds at bp+1.
                let right = self.expr_bp(bp + 1)?;
                Ok(Expr::Binary {
                    op: b,
                    left: Box::new(left),
                    right: Box::new(right),
                })
            }
            InfixOp::Is => {
                self.pos += 1; // IS
                let negated = self.eat_kw("not");
                if self.eat_kw("null") {
                    return Ok(Expr::IsNull {
                        expr: Box::new(left),
                        negated,
                    });
                }
                // `IS [NOT] DISTINCT FROM` is null-aware (in)equality: `IS DISTINCT
                // FROM` == `IS NOT`, `IS NOT DISTINCT FROM` == `IS`.
                let distinct = self.eat_kw("distinct");
                if distinct {
                    self.expect_kw("from")?;
                }
                let right = self.expr_bp(bp + 1)?;
                let equality = if distinct { negated } else { !negated };
                Ok(Expr::Binary {
                    op: if equality {
                        BinaryOp::Is
                    } else {
                        BinaryOp::IsNot
                    },
                    left: Box::new(left),
                    right: Box::new(right),
                })
            }
            InfixOp::IsNullKw { negated } => {
                self.pos += 1;
                Ok(Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                })
            }
            InfixOp::In { negated } => {
                self.pos += 1; // IN
                self.parse_in(left, negated)
            }
            InfixOp::Between { negated } => {
                self.pos += 1; // BETWEEN
                self.parse_between(left, negated)
            }
            InfixOp::Like { negated } => {
                self.pos += 1; // LIKE
                self.parse_like(left, negated)
            }
            InfixOp::NotPrefixed => {
                // NOT IN / NOT LIKE / NOT GLOB / NOT BETWEEN
                self.pos += 1; // NOT
                if self.eat_kw("in") {
                    self.parse_in(left, true)
                } else if self.eat_kw("between") {
                    self.parse_between(left, true)
                } else if self.eat_kw("like") {
                    self.parse_like(left, true)
                } else if self.eat_kw("glob") {
                    let right = self.expr_bp(bp + 1)?;
                    Ok(Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(Expr::Binary {
                            op: BinaryOp::Glob,
                            left: Box::new(left),
                            right: Box::new(right),
                        }),
                    })
                } else {
                    Err(self.err("expected IN/LIKE/GLOB/BETWEEN after NOT"))
                }
            }
        }
    }

    /// Parse the right-hand side of `text LIKE pattern [ESCAPE c]`. Without
    /// `ESCAPE` this is `BinaryOp::Like`; with it, the SQLite 3-argument
    /// `like(pattern, text, escape)` function form. `NOT LIKE` wraps in `NOT`.
    fn parse_like(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        let pattern = self.expr_bp(BP_EQ + 1)?;
        let escape = if self.eat_kw("escape") {
            Some(self.expr_bp(BP_EQ + 1)?)
        } else {
            None
        };
        let core = match escape {
            None => Expr::Binary {
                op: BinaryOp::Like,
                left: Box::new(left),
                right: Box::new(pattern),
            },
            Some(esc) => Expr::Function {
                name: String::from("like"),
                distinct: false,
                args: alloc::vec![pattern, left, esc], // like(pattern, text, escape)
                star: false,
                filter: None,
                order_by: Vec::new(),
                over: None,
            },
        };
        if negated {
            Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(core),
            })
        } else {
            Ok(core)
        }
    }

    fn parse_in(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        self.expect(&Token::LParen)?;
        // `IN (SELECT …)` vs `IN (v1, v2, …)`.
        if self.check_kw("select") || self.check_kw("with") {
            let sel = self.select()?;
            self.expect(&Token::RParen)?;
            return Ok(Expr::InSelect {
                expr: Box::new(left),
                select: Box::new(sel),
                negated,
            });
        }
        let mut list = Vec::new();
        if !self.check(&Token::RParen) {
            list.push(self.expr()?);
            while self.eat(&Token::Comma) {
                list.push(self.expr()?);
            }
        }
        self.expect(&Token::RParen)?;
        Ok(Expr::InList {
            expr: Box::new(left),
            list,
            negated,
        })
    }

    fn parse_between(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        // Operands bind tighter than AND so BETWEEN's AND is not the boolean one.
        let low = self.expr_bp(BP_BIT)?;
        self.expect_kw("and")?;
        let high = self.expr_bp(BP_BIT)?;
        Ok(Expr::Between {
            expr: Box::new(left),
            low: Box::new(low),
            high: Box::new(high),
            negated,
        })
    }
}

/// Classification of an infix position for the Pratt loop.
#[derive(Debug, Clone, Copy)]
enum InfixOp {
    Binary(BinaryOp),
    Is,
    IsNullKw { negated: bool },
    In { negated: bool },
    Between { negated: bool },
    Like { negated: bool },
    NotPrefixed,
}

/// Build the function expression a `CURRENT_DATE`/`CURRENT_TIME`/
/// `CURRENT_TIMESTAMP` keyword desugars to: `<fn>('now')`.
fn now_datetime_fn(func: &str) -> Expr {
    Expr::Function {
        name: String::from(func),
        distinct: false,
        args: alloc::vec![Expr::Literal(Literal::Str(String::from("now")))],
        star: false,
        filter: None,
        order_by: alloc::vec![],
        over: None,
    }
}

/// Keywords that end an expression in a result-column/table context, so a bare
/// word here is a clause keyword rather than an implicit alias.
fn is_reserved_after_expr(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "from"
            | "where"
            | "group"
            | "having"
            | "order"
            | "limit"
            | "offset"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "full"
            | "outer"
            | "cross"
            | "natural"
            | "on"
            | "using"
            | "and"
            | "or"
            | "as"
            | "when"
            | "then"
            | "else"
            | "end"
            | "union"
            | "intersect"
            | "except"
            | "window"
            | "indexed"
            | "not"
    )
}

/// Keywords that cannot appear where an expression primary is expected. These
/// are the SQLite reserved words that are never usable as bare identifiers in
/// expression position (so `SELECT FROM` is a syntax error, not a column named
/// `FROM`). The list is intentionally conservative — words SQLite allows as
/// identifiers (e.g. `key`, `default` in some contexts) are not included.
fn is_reserved_keyword(lower: &str) -> bool {
    matches!(
        lower,
        "select"
            | "from"
            | "where"
            | "group"
            | "having"
            | "order"
            | "limit"
            | "offset"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "cross"
            | "on"
            | "using"
            | "as"
            | "when"
            | "then"
            | "else"
            | "end"
            | "into"
            | "values"
            | "set"
            | "by"
            | "and"
            | "or"
            | "insert"
            | "update"
            | "delete"
            | "create"
            | "drop"
            | "table"
            | "union"
            | "intersect"
            | "except"
    )
}

fn is_column_constraint_kw(tok: Option<&Token>) -> bool {
    matches!(tok, Some(Token::Word(w)) if matches!(
        w.to_ascii_lowercase().as_str(),
        "primary" | "not" | "null" | "unique" | "default" | "collate" | "check"
            | "references" | "generated" | "as"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn one(sql: &str) -> Statement {
        parse_one(sql).unwrap()
    }

    #[test]
    fn create_virtual_table() {
        let Statement::CreateVirtualTable(cvt) = one("CREATE VIRTUAL TABLE v USING series(1, 5)")
        else {
            panic!()
        };
        assert!(!cvt.if_not_exists);
        assert_eq!(cvt.name, "v");
        assert_eq!(cvt.module, "series");
        assert_eq!(cvt.args, vec![String::from("1"), String::from("5")]);

        // IF NOT EXISTS, a negative argument, and no parens.
        let Statement::CreateVirtualTable(cvt) =
            one("CREATE VIRTUAL TABLE IF NOT EXISTS s USING series(-3, 3, 2)")
        else {
            panic!()
        };
        assert!(cvt.if_not_exists);
        assert_eq!(
            cvt.args,
            vec![String::from("-3"), String::from("3"), String::from("2")]
        );

        let Statement::CreateVirtualTable(cvt) = one("CREATE VIRTUAL TABLE m USING mod") else {
            panic!()
        };
        assert_eq!(cvt.module, "mod");
        assert!(cvt.args.is_empty());
    }

    #[test]
    fn simple_select() {
        let s = one("SELECT a, b AS bee, t.* FROM t WHERE a > 1 ORDER BY b DESC LIMIT 10");
        let Statement::Select(sel) = s else { panic!() };
        assert_eq!(sel.columns.len(), 3);
        assert!(sel.where_clause.is_some());
        assert_eq!(sel.order_by.len(), 1);
        assert!(sel.order_by[0].descending);
        assert!(sel.limit.is_some());
    }

    #[test]
    fn select_star() {
        let Statement::Select(sel) = one("select * from t") else {
            panic!()
        };
        assert_eq!(sel.columns, vec![ResultColumn::Wildcard]);
        assert_eq!(sel.from.unwrap().first.name, "t");
    }

    #[test]
    fn expression_precedence() {
        // 1 + 2 * 3 = 7, and AND binds looser than comparison.
        let Statement::Select(sel) = one("SELECT 1 + 2 * 3 WHERE a = 1 AND b = 2") else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &sel.columns[0] else {
            panic!()
        };
        // Top of the projected expr must be Add, with Mul on the right.
        let Expr::Binary {
            op: BinaryOp::Add,
            right,
            ..
        } = expr
        else {
            panic!("expected Add at top, got {expr:?}")
        };
        assert!(matches!(
            **right,
            Expr::Binary {
                op: BinaryOp::Mul,
                ..
            }
        ));
        // WHERE must be (a=1) AND (b=2).
        let Some(Expr::Binary {
            op: BinaryOp::And, ..
        }) = sel.where_clause
        else {
            panic!("expected AND at top of WHERE")
        };
    }

    #[test]
    fn in_between_is_null_like() {
        one("SELECT * FROM t WHERE a IN (1,2,3)");
        one("SELECT * FROM t WHERE a NOT IN (1,2)");
        one("SELECT * FROM t WHERE a BETWEEN 1 AND 10");
        one("SELECT * FROM t WHERE a NOT BETWEEN 1 AND 10");
        one("SELECT * FROM t WHERE a IS NULL");
        one("SELECT * FROM t WHERE a IS NOT NULL");
        one("SELECT * FROM t WHERE name LIKE 'a%'");
    }

    #[test]
    fn functions_and_case_and_cast() {
        one("SELECT count(*), max(a), substr(b,1,2) FROM t");
        one("SELECT count(DISTINCT a) FROM t");
        one("SELECT CASE WHEN a > 0 THEN 'p' WHEN a < 0 THEN 'n' ELSE 'z' END FROM t");
        one("SELECT CAST(a AS TEXT) FROM t");
    }

    #[test]
    fn insert_forms() {
        let Statement::Insert(ins) = one("INSERT INTO t(a,b) VALUES (1,'x'),(2,'y')") else {
            panic!()
        };
        assert_eq!(ins.columns, vec!["a", "b"]);
        match ins.source {
            InsertSource::Values(rows) => assert_eq!(rows.len(), 2),
            _ => panic!(),
        }
        one("INSERT INTO t DEFAULT VALUES");
        one("INSERT INTO t SELECT * FROM u");
        one("INSERT OR REPLACE INTO t VALUES (1)");
    }

    #[test]
    fn update_delete() {
        let Statement::Update(u) = one("UPDATE t SET a = 1, b = a + 1 WHERE id = 5") else {
            panic!()
        };
        assert_eq!(u.assignments.len(), 2);
        assert!(u.where_clause.is_some());

        let Statement::Delete(d) = one("DELETE FROM t WHERE a < 0") else {
            panic!()
        };
        assert_eq!(d.table, "t");
    }

    #[test]
    fn create_table_and_index() {
        let Statement::CreateTable(ct) =
            one("CREATE TABLE IF NOT EXISTS t(a INTEGER PRIMARY KEY, b TEXT NOT NULL, c REAL DEFAULT 0)")
        else {
            panic!()
        };
        assert!(ct.if_not_exists);
        assert_eq!(ct.columns.len(), 3);
        assert_eq!(ct.columns[0].type_name.as_deref(), Some("INTEGER"));
        assert!(ct.columns[0]
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::PrimaryKey { .. })));

        let Statement::CreateIndex(ci) = one("CREATE UNIQUE INDEX idx ON t(b, c DESC)") else {
            panic!()
        };
        assert!(ci.unique);
        assert_eq!(ci.columns.len(), 2);
        assert!(ci.columns[1].descending);
    }

    #[test]
    fn create_table_with_table_constraint() {
        let Statement::CreateTable(ct) = one("CREATE TABLE t(a, b, PRIMARY KEY(a, b))") else {
            panic!()
        };
        assert_eq!(ct.columns.len(), 2);
        assert_eq!(ct.constraints.len(), 1);
    }

    #[test]
    fn create_table_full_constraint_grammar() {
        // Real-world constraints (CHECK, REFERENCES, FOREIGN KEY, named
        // CONSTRAINT, conflict clauses) must parse so stored schemas load.
        let Statement::CreateTable(ct) = one("CREATE TABLE child(\
               id INTEGER PRIMARY KEY AUTOINCREMENT, \
               pid INT REFERENCES parent(id) ON DELETE CASCADE, \
               qty INT NOT NULL CHECK(qty > 0) DEFAULT 1, \
               name TEXT COLLATE NOCASE UNIQUE ON CONFLICT IGNORE, \
               CONSTRAINT uq UNIQUE(pid, qty), \
               FOREIGN KEY(pid) REFERENCES parent(id))")
        else {
            panic!()
        };
        assert_eq!(ct.columns.len(), 4); // id, pid, qty, name
        assert_eq!(ct.columns[0].name, "id");
        assert!(ct.columns[2]
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::NotNull)));
        // The column-level REFERENCES on `pid` is captured with its action.
        let pid_fk = ct.columns[1]
            .constraints
            .iter()
            .find_map(|c| match c {
                ColumnConstraint::References(fk) => Some(fk),
                _ => None,
            })
            .expect("pid REFERENCES captured");
        assert_eq!(pid_fk.ref_table, "parent");
        assert_eq!(pid_fk.on_delete, FkAction::Cascade);
        // Table constraints: the UNIQUE and the table-level FOREIGN KEY.
        assert_eq!(ct.constraints.len(), 2);
        assert!(ct
            .constraints
            .iter()
            .any(|c| matches!(c, TableConstraint::Unique(_))));
        assert!(ct
            .constraints
            .iter()
            .any(|c| matches!(c, TableConstraint::ForeignKey(_))));
    }

    #[test]
    fn drop_and_tx_and_pragma() {
        assert!(matches!(one("DROP TABLE IF EXISTS t"), Statement::Drop(_)));
        assert!(matches!(one("BEGIN"), Statement::Begin));
        assert!(matches!(one("COMMIT"), Statement::Commit));
        assert!(matches!(one("ROLLBACK"), Statement::Rollback));
        let Statement::Pragma(p) = one("PRAGMA page_size = 4096") else {
            panic!()
        };
        assert_eq!(p.name, "page_size");
        assert!(p.value.is_some());
    }

    #[test]
    fn multiple_statements() {
        let stmts = parse("CREATE TABLE t(a); INSERT INTO t VALUES (1); SELECT * FROM t;").unwrap();
        assert_eq!(stmts.len(), 3);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("SELECT FROM").is_err());
        assert!(parse("INSERT INTO").is_err());
        assert!(parse("!!!").is_err());
    }
}
