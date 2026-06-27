//! `graphitesql` — an interactive shell over the graphitesql engine, modeled on
//! the `sqlite3` command-line tool.
//!
//! Usage:
//!
//! ```text
//! graphitesql                 # in-memory database, interactive
//! graphitesql FILE            # open (or create) FILE, interactive
//! graphitesql FILE "SQL..."   # run SQL against FILE and exit
//! graphitesql :memory: "SQL"  # run SQL in memory and exit
//! ```
//!
//! Interactive input accepts SQL statements terminated by `;` (across multiple
//! lines) and a handful of `.dot` commands (`.help` lists them). Query results
//! print in SQLite's default "list" mode: columns joined by `|`, one row per
//! line.

use graphitesql::{Connection, QueryResult, Value};
use std::io::{self, BufRead, IsTerminal, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // First argument (if any) is the database path; each remaining argument is an
    // independent one-shot SQL batch, run in order then exit. The `sqlite3` CLI
    // runs each trailing argument as its own statement(s), so `db "SELECT 1"
    // "SELECT 2"` executes both even though neither ends in `;` — joining them
    // into one string would instead splice `1SELECT` into a syntax error.
    let (path, scripts) = match args.split_first() {
        None => (String::from(":memory:"), &[][..]),
        Some((db, rest)) => (db.clone(), rest),
    };

    let mut conn = match open(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: unable to open {path:?}: {e}");
            std::process::exit(1);
        }
    };

    let mut shell = Shell { headers: false };

    if !scripts.is_empty() {
        // One-shot mode: run each argument batch, exiting non-zero on the first
        // error (like the `sqlite3` shell).
        for sql in scripts {
            if let Err(e) = shell.run_sql_batch(&mut conn, sql) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    shell.repl(&mut conn, &path);
}

/// Open `path`: `:memory:` (or empty) for in-memory, an existing file read/write,
/// or a new file created on demand.
fn open(path: &str) -> graphitesql::Result<Connection> {
    if path.is_empty() || path == ":memory:" {
        Connection::open_memory()
    } else if std::path::Path::new(path).exists() {
        Connection::open(path)
    } else {
        Connection::create(path)
    }
}

struct Shell {
    /// Whether to print a header row before query results (`.headers on`).
    headers: bool,
}

impl Shell {
    fn repl(&mut self, conn: &mut Connection, path: &str) {
        let interactive = io::stdin().is_terminal();
        if interactive {
            eprintln!("graphitesql shell — connected to {path}");
            eprintln!(
                "Enter SQL statements ending in ';'. \".help\" for commands, \".quit\" to exit."
            );
        }
        let stdin = io::stdin();
        let mut buffer = String::new();

        loop {
            if interactive {
                let prompt = if buffer.is_empty() {
                    "graphitesql> "
                } else {
                    "        ...> "
                };
                print!("{prompt}");
                let _ = io::stdout().flush();
            }

            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Error reading input: {e}");
                    break;
                }
            }

            let trimmed = line.trim();
            // Dot-commands are only recognized at the start of a fresh buffer.
            if buffer.is_empty() && trimmed.starts_with('.') {
                if self.dot_command(conn, trimmed) {
                    break;
                }
                continue;
            }

            buffer.push_str(&line);
            // Execute once the accumulated input contains a complete statement.
            if buffer.trim_end().ends_with(';') {
                let sql = std::mem::take(&mut buffer);
                if let Err(e) = self.run_sql_batch(conn, &sql) {
                    eprintln!("Error: {e}");
                }
            }
        }
    }

    /// Run one or more `;`-separated statements.
    fn run_sql_batch(&mut self, conn: &mut Connection, sql: &str) -> graphitesql::Result<()> {
        for stmt in split_statements(sql) {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            self.run_one(conn, stmt)?;
        }
        Ok(())
    }

    fn run_one(&mut self, conn: &mut Connection, sql: &str) -> graphitesql::Result<()> {
        // A `PRAGMA name = value` setter must go through `execute` (`&mut self`):
        // `query` takes `&self` and cannot mutate connection state, so routing a
        // setter through it would silently no-op (e.g. `PRAGMA foreign_keys=ON`).
        // Getter pragmas (`PRAGMA foreign_keys`, `PRAGMA table_info(t)`) have no
        // `=` and still return rows via `query`.
        // `returns_rows`/`is_pragma_setter` are first-word heuristics and can
        // misroute: a `WITH …`-prefixed statement may be DML (INSERT/UPDATE/
        // DELETE), and `EXPLAIN` returns rows. When the engine reports the wrong
        // method was used, retry with the other one.
        if returns_rows(sql) && !is_pragma_setter(sql) {
            match conn.query(sql) {
                // EXPLAIN QUERY PLAN renders as SQLite's `QUERY PLAN` tree rather
                // than the raw (id, parent, notused, detail) rows.
                Ok(result) if is_explain_query_plan(sql) => self.print_eqp_tree(&result),
                Ok(result) => self.print_result(&result),
                Err(graphitesql::Error::Unsupported(m)) if m.contains("use execute()") => {
                    // A `WITH …`-prefixed DML statement was misrouted to query();
                    // run it as a mutation. If it also has RETURNING, project the
                    // rows via execute_returning rather than discarding them.
                    if has_returning(sql) {
                        let result = conn
                            .execute_returning(sql, &graphitesql::exec::eval::Params::default())?;
                        self.print_result(&result);
                    } else {
                        conn.execute(sql)?;
                    }
                }
                Err(e) => return Err(e),
            }
        } else if has_returning(sql) {
            // INSERT/UPDATE/DELETE … RETURNING mutates *and* projects rows; run it
            // via execute_returning and print the projected rows.
            let result =
                conn.execute_returning(sql, &graphitesql::exec::eval::Params::default())?;
            self.print_result(&result);
        } else {
            match conn.execute(sql) {
                Ok(_) => {}
                Err(graphitesql::Error::Unsupported(m)) if m.contains("use query()") => {
                    let result = conn.query(sql)?;
                    self.print_result(&result);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn print_result(&self, result: &QueryResult) {
        let out = io::stdout();
        let mut out = out.lock();
        if self.headers {
            let _ = writeln!(out, "{}", result.columns.join("|"));
        }
        // Write rows as bytes, not as a String: a BLOB column prints its raw
        // bytes (which need not be valid UTF-8) in list mode, matching the
        // sqlite3 CLI — e.g. `SELECT x'48656c6c6f'` prints `Hello`, not its hex.
        let mut line: Vec<u8> = Vec::new();
        for row in &result.rows {
            line.clear();
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    line.push(b'|');
                }
                render_value_into(v, &mut line);
            }
            line.push(b'\n');
            let _ = out.write_all(&line);
        }
    }

    /// Render an EXPLAIN QUERY PLAN result as SQLite's `QUERY PLAN` tree. The
    /// rows are `(id, parent, notused, detail)`; children link to their parent's
    /// `id` (top-level rows have parent 0). The last child of a node uses `` `-- ``
    /// (others `|--`), with `   ` / `|  ` continuation indent.
    fn print_eqp_tree(&self, result: &QueryResult) {
        let nodes: Vec<(i64, i64, String)> = result
            .rows
            .iter()
            .filter_map(|r| {
                let id = match r.first() {
                    Some(Value::Integer(i)) => *i,
                    _ => return None,
                };
                let parent = match r.get(1) {
                    Some(Value::Integer(i)) => *i,
                    _ => return None,
                };
                let detail = match r.last() {
                    Some(Value::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                Some((id, parent, detail))
            })
            .collect();
        let out = io::stdout();
        let mut out = out.lock();
        let _ = writeln!(out, "QUERY PLAN");
        render_eqp(&mut out, &nodes, 0, "");
    }

    /// Handle a `.dot` command. Returns `true` if the shell should exit.
    fn dot_command(&mut self, conn: &mut Connection, line: &str) -> bool {
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next();
        match cmd {
            ".quit" | ".exit" => return true,
            ".help" => print_help(),
            ".tables" => {
                for obj in conn.schema().objects() {
                    if obj.obj_type == graphitesql::schema::ObjectType::Table {
                        println!("{}", obj.name);
                    }
                }
            }
            ".schema" => {
                for obj in conn.schema().objects() {
                    if arg.is_none_or(|name| name == obj.name) {
                        if let Some(sql) = &obj.sql {
                            println!("{sql};");
                        }
                    }
                }
            }
            ".headers" => match arg {
                Some("on") => self.headers = true,
                Some("off") => self.headers = false,
                _ => eprintln!("Usage: .headers on|off"),
            },
            ".mode" => { /* accepted for compatibility; only list mode is supported */ }
            other => eprintln!("Unknown command: {other}. Try \".help\"."),
        }
        false
    }
}

fn print_help() {
    eprintln!(".help              Show this message");
    eprintln!(".tables            List table names");
    eprintln!(".schema [TABLE]    Show CREATE statements");
    eprintln!(".headers on|off    Toggle column headers (default off)");
    eprintln!(".quit / .exit      Exit the shell");
}

/// Render a value the way the `sqlite3` shell does in list mode, appending its
/// bytes to `out` (NULL prints as the empty string).
///
/// A BLOB emits its raw bytes verbatim — they need not be valid UTF-8 — but, as
/// the `sqlite3` CLI does, only up to the first NUL: list mode hands the blob to
/// C string routines, so `x'410042'` prints as `A`, and `x'00ff'` prints as
/// nothing.
fn render_value_into(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => {}
        Value::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        // Use the engine's canonical real rendering (15 significant digits with
        // `%g`-style fixed/exponential switching, `Inf`/`-Inf`, `0.0` for zero) so
        // the shell matches the `sqlite3` CLI for large/small/special magnitudes —
        // e.g. `1e18` -> `1.0e+18`, `1e400` -> `Inf`, `-0.0` -> `0.0`.
        Value::Real(r) => {
            out.extend_from_slice(graphitesql::exec::eval::format_real(*r).as_bytes())
        }
        // Both TEXT and BLOB stop at the first NUL: list mode renders through C
        // string routines, so `'A'||char(0)||'B'` and `x'410042'` both print `A`.
        Value::Text(s) => {
            let b = s.as_bytes();
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            out.extend_from_slice(&b[..end]);
        }
        Value::Blob(b) => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            out.extend_from_slice(&b[..end]);
        }
    }
}

/// Whether a statement produces a result set worth printing.
fn returns_rows(sql: &str) -> bool {
    let word = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        word.as_str(),
        "SELECT" | "PRAGMA" | "WITH" | "VALUES" | "EXPLAIN"
    )
}

/// Recursively render the children of `parent` in an EXPLAIN QUERY PLAN tree.
fn render_eqp(out: &mut dyn Write, nodes: &[(i64, i64, String)], parent: i64, prefix: &str) {
    let children: Vec<&(i64, i64, String)> =
        nodes.iter().filter(|(_, p, _)| *p == parent).collect();
    let last_i = children.len().wrapping_sub(1);
    for (i, (id, _, detail)) in children.iter().enumerate() {
        let last = i == last_i;
        let connector = if last { "`--" } else { "|--" };
        let _ = writeln!(out, "{prefix}{connector}{detail}");
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "|  " });
        render_eqp(out, nodes, *id, &child_prefix);
    }
}

/// Whether `sql` is an `EXPLAIN QUERY PLAN …` statement (rendered as a tree).
fn is_explain_query_plan(sql: &str) -> bool {
    let mut words = sql.split_whitespace();
    words
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("explain"))
        && words
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("query"))
        && words.next().is_some_and(|w| w.eq_ignore_ascii_case("plan"))
}

/// Whether `sql` contains a `RETURNING` keyword (as a whole word, outside string
/// literals) — a CLI heuristic to route INSERT/UPDATE/DELETE … RETURNING to
/// `execute_returning` so the projected rows are printed.
fn has_returning(sql: &str) -> bool {
    let mut in_str = false;
    let mut word = String::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    in_str = false;
                }
            }
            continue;
        }
        if c == '\'' {
            in_str = true;
        } else if c.is_alphabetic() || c == '_' {
            word.push(c);
        } else {
            if word.eq_ignore_ascii_case("returning") {
                return true;
            }
            word.clear();
        }
    }
    word.eq_ignore_ascii_case("returning")
}

/// Whether `sql` is a `PRAGMA name = value` setter (which mutates connection
/// state and so must run through `execute`, not `query`). The `= value` form is
/// the distinguishing mark; bare/`(arg)` getter pragmas have no `=`.
fn is_pragma_setter(sql: &str) -> bool {
    let word = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_ascii_uppercase();
    word == "PRAGMA" && sql.contains('=')
}

/// Split a batch into statements on `;`, respecting single-quoted strings so a
/// `;` inside a literal does not split it. (Good enough for a shell; the engine
/// re-parses each piece.)
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    // Block nesting for `BEGIN … END` (trigger bodies) and `CASE … END`: a `;`
    // inside one does not end the statement. A *transaction* `BEGIN`/`END` is a
    // standalone statement (the first word, or with `depth == 0`), so it does not
    // open/close a block — only a mid-statement `BEGIN`/`CASE` does.
    let mut depth: u32 = 0;
    let mut word = String::new();
    let mut chars = sql.chars().peekable();
    let flush_word = |word: &mut String, cur: &str, depth: &mut u32| {
        match word.to_ascii_uppercase().as_str() {
            // A mid-statement BEGIN (content precedes it, e.g. `CREATE TRIGGER …
            // BEGIN`) opens a trigger body; a *leading* BEGIN is a transaction
            // statement. `cur` already ends with this word, so look at what comes
            // before it.
            "BEGIN" => {
                let before = &cur[..cur.len().saturating_sub(word.len())];
                if !before.trim().is_empty() {
                    *depth += 1;
                }
            }
            "CASE" => *depth += 1,
            "END" => *depth = depth.saturating_sub(1),
            _ => {}
        }
        word.clear();
    };
    while let Some(c) = chars.next() {
        if in_str {
            cur.push(c);
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    cur.push(chars.next().unwrap());
                } else {
                    in_str = false;
                }
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            word.push(c);
            cur.push(c);
            continue;
        }
        // Word boundary: classify the keyword just read.
        if !word.is_empty() {
            flush_word(&mut word, &cur, &mut depth);
        }
        match c {
            '\'' => {
                in_str = true;
                cur.push(c);
            }
            ';' if depth == 0 => {
                // Re-attach the terminating `;` so the engine can tell a
                // `;`-truncated statement (`SELECT;` → `near ";": syntax error`)
                // apart from a genuine end-of-input truncation (`SELECT` at EOF →
                // `incomplete input`), exactly as SQLite's CLI does. A bare/blank
                // `;` stays empty so `run_sql_batch` skips it as a no-op.
                if !cur.trim().is_empty() {
                    cur.push(';');
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !word.is_empty() {
        flush_word(&mut word, &cur, &mut depth);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}
