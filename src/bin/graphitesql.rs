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

    /// Process lines from `reader` exactly as the interactive loop does (minus the
    /// prompts): a line beginning a fresh buffer with `.` is a dot-command, others
    /// accumulate into an SQL statement that runs at each terminating `;`. Used by
    /// the `.read` command. Returns `true` if a `.quit`/`.exit` was reached.
    fn feed_reader(&mut self, conn: &mut Connection, reader: &mut impl BufRead) -> bool {
        let mut buffer = String::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Error reading input: {e}");
                    break;
                }
            }
            let trimmed = line.trim();
            if buffer.is_empty() && trimmed.starts_with('.') {
                if self.dot_command(conn, trimmed) {
                    return true;
                }
                continue;
            }
            buffer.push_str(&line);
            if buffer.trim_end().ends_with(';') {
                let sql = std::mem::take(&mut buffer);
                if let Err(e) = self.run_sql_batch(conn, &sql) {
                    eprintln!("Error: {e}");
                }
            }
        }
        false
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
                Ok(_) => {
                    // `PRAGMA journal_mode = X` is a setter that still reports the
                    // resulting journal mode — SQLite prints it (e.g. `wal`, or
                    // `memory` for an in-memory database that cannot change it).
                    // The side effect ran through execute(); read the mode back via
                    // the getter and print it, matching SQLite's output.
                    if let Some(getter) = pragma_setter_result_query(sql)
                        && let Ok(result) = conn.query(&getter)
                    {
                        self.print_result(&result);
                    }
                }
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
                // Tables and views, excluding internal `sqlite_*`; an optional
                // argument is used verbatim as a `LIKE` pattern on the name.
                let mut sql = String::from(
                    "SELECT name FROM sqlite_master \
                     WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%'",
                );
                if let Some(pat) = arg {
                    sql.push_str(&format!(" AND name LIKE '{}'", pat.replace('\'', "''")));
                }
                print_columnar(collect_names(conn, &sql));
            }
            ".indexes" | ".indices" => {
                // Every index (including auto-created UNIQUE/PK indexes); an
                // optional argument filters by owning table via `LIKE`.
                let mut sql = String::from("SELECT name FROM sqlite_master WHERE type='index'");
                if let Some(pat) = arg {
                    sql.push_str(&format!(" AND tbl_name LIKE '{}'", pat.replace('\'', "''")));
                }
                print_columnar(collect_names(conn, &sql));
            }
            ".schema" => {
                use graphitesql::schema::ObjectType;
                for obj in conn.schema().objects() {
                    if arg.is_none_or(|name| name == obj.name)
                        && let Some(sql) = &obj.sql
                    {
                        let base = schema_create_line(sql);
                        // SQLite's `.schema` annotates a view with its output
                        // column names on a trailing comment line.
                        if obj.obj_type == ObjectType::View {
                            println!(
                                "{base}\n/* {}({}) */;",
                                obj.name,
                                view_columns(conn, &obj.name)
                            );
                        } else {
                            println!("{base};");
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
            ".databases" => {
                // `<name>: <file-or-""> r/w` per attached database (from
                // `PRAGMA database_list`). The shell opens read/write, and no
                // transaction is active while a dot-command runs, so the mode is
                // always `r/w` with no transaction annotation here.
                if let Ok(r) = conn.query("PRAGMA database_list") {
                    for row in &r.rows {
                        let name = match row.get(1) {
                            Some(Value::Text(s)) => s.as_str(),
                            _ => "",
                        };
                        let file = match row.get(2) {
                            Some(Value::Text(s)) if !s.is_empty() => s.as_str(),
                            _ => "\"\"",
                        };
                        println!("{name}: {file} r/w");
                    }
                }
            }
            ".dump" => dump_database(conn),
            ".read" => {
                // The filename is the rest of the line (so paths may contain
                // spaces), stripped of surrounding whitespace.
                let file = line.strip_prefix(".read").map(str::trim).unwrap_or("");
                if file.is_empty() {
                    eprintln!("Usage: .read FILE");
                } else {
                    match std::fs::File::open(file) {
                        Ok(f) => {
                            let mut r = io::BufReader::new(f);
                            if self.feed_reader(conn, &mut r) {
                                return true; // a `.quit` inside the file exits
                            }
                        }
                        Err(_) => eprintln!("Error: cannot open \"{file}\""),
                    }
                }
            }
            other => eprintln!("Unknown command: {other}. Try \".help\"."),
        }
        false
    }
}

fn print_help() {
    eprintln!(".help              Show this message");
    eprintln!(".tables [LIKE]     List table and view names");
    eprintln!(".indexes [LIKE]    List index names");
    eprintln!(".schema [TABLE]    Show CREATE statements");
    eprintln!(".databases         List attached databases");
    eprintln!(".dump              Dump the database as SQL text");
    eprintln!(".read FILE         Execute SQL from FILE");
    eprintln!(".headers on|off    Toggle column headers (default off)");
    eprintln!(".quit / .exit      Exit the shell");
}

/// Emit the whole database as a stream of SQL text, matching `sqlite3`'s `.dump`:
/// a `PRAGMA foreign_keys=OFF;` / `BEGIN TRANSACTION;` header, each table's
/// `CREATE` followed by an `INSERT` per row, then the indexes/triggers/views, and
/// a closing `COMMIT;`. Internal `sqlite_*` objects and the auto-created
/// `sqlite_autoindex_*` indexes (which have no backing SQL) are skipped, as in
/// SQLite.
fn dump_database(conn: &Connection) {
    println!("PRAGMA foreign_keys=OFF;");
    println!("BEGIN TRANSACTION;");
    // Pass 1: tables, each immediately followed by its data.
    for obj in conn.schema().objects() {
        if obj.obj_type != graphitesql::schema::ObjectType::Table {
            continue;
        }
        if obj.name.starts_with("sqlite_") {
            continue;
        }
        let Some(sql) = &obj.sql else { continue };
        println!("{};", schema_create_line(sql));
        // `INSERT ... VALUES(...)` targets only the stored columns — a generated
        // column (`hidden` 2 = virtual, 3 = stored) has no value to write — so
        // select exactly those, in declared order, rather than `SELECT *`.
        let xinfo = format!(
            "SELECT name FROM pragma_table_xinfo('{}') WHERE hidden NOT IN (2,3)",
            obj.name.replace('\'', "''")
        );
        let col_names: Vec<String> = match conn.query(&xinfo) {
            Ok(r) => r
                .rows
                .iter()
                .filter_map(|row| match row.first() {
                    Some(Value::Text(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            Err(_) => continue,
        };
        if col_names.is_empty() {
            continue;
        }
        let select_list = col_names
            .iter()
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(",");
        let sel = format!(
            "SELECT {select_list} FROM \"{}\"",
            obj.name.replace('"', "\"\"")
        );
        if let Ok(result) = conn.query(&sel) {
            for row in &result.rows {
                let mut line = format!("INSERT INTO {} VALUES(", quote_ident_if_needed(&obj.name));
                for (i, v) in row.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    dump_value_into(v, &mut line);
                }
                line.push_str(");");
                println!("{line}");
            }
        }
    }
    // AUTOINCREMENT tables keep a high-water mark in `sqlite_sequence`; SQLite
    // dumps that table's rows (right after the user tables) so the sequence is
    // restored on reload. There is no CREATE — it is auto-created on demand.
    if conn.schema().table("sqlite_sequence").is_some()
        && let Ok(result) = conn.query("SELECT name, seq FROM sqlite_sequence")
    {
        for row in &result.rows {
            let mut line = String::from("INSERT INTO sqlite_sequence VALUES(");
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    line.push(',');
                }
                dump_value_into(v, &mut line);
            }
            line.push_str(");");
            println!("{line}");
        }
    }
    // Pass 2: indexes, triggers, and views, after all table data. SQLite emits
    // them `ORDER BY type COLLATE NOCASE DESC` — i.e. views, then triggers, then
    // indexes — preserving creation (rowid) order within each type.
    use graphitesql::schema::ObjectType;
    let type_rank = |t: ObjectType| match t {
        ObjectType::View => 0,
        ObjectType::Trigger => 1,
        ObjectType::Index => 2,
        ObjectType::Table => 3,
    };
    let mut rest: Vec<_> = conn
        .schema()
        .objects()
        .iter()
        .filter(|o| !matches!(o.obj_type, ObjectType::Table) && !o.name.starts_with("sqlite_"))
        .collect();
    rest.sort_by_key(|o| type_rank(o.obj_type));
    for obj in rest {
        if let Some(sql) = &obj.sql {
            println!("{sql};");
        }
    }
    println!("COMMIT;");
}

/// Collect the single-column text results of `sql` (a name-listing query).
fn collect_names(conn: &Connection, sql: &str) -> Vec<String> {
    match conn.query(sql) {
        Ok(r) => r
            .rows
            .iter()
            .filter_map(|row| match row.first() {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Print names in SQLite's `.tables`/`.indexes` columnar layout: sorted (byte
/// order), laid out column-major into `80/(maxlen+2)` columns each `maxlen` wide,
/// left-justified, with a two-space gap between columns. Nothing is printed for an
/// empty list.
fn print_columnar(mut names: Vec<String>) {
    if names.is_empty() {
        return;
    }
    names.sort();
    let maxlen = names.iter().map(String::len).max().unwrap_or(0);
    let n_col = (80 / (maxlen + 2)).max(1);
    let n_row = names.len().div_ceil(n_col);
    for i in 0..n_row {
        let mut line = String::new();
        let mut j = i;
        while j < names.len() {
            if j >= n_row {
                line.push_str("  ");
            }
            line.push_str(&format!("{:<maxlen$}", names[j]));
            j += n_row;
        }
        println!("{line}");
    }
}

/// The comma-separated output column names of a view, for SQLite's `.schema`
/// `/* view(cols) */` annotation. Empty (so the comment is `/* v() */`) if the
/// view cannot be introspected.
fn view_columns(conn: &Connection, name: &str) -> String {
    conn.query(&format!(
        "SELECT name FROM pragma_table_info('{}')",
        name.replace('\'', "''")
    ))
    .map(|r| {
        r.rows
            .iter()
            .filter_map(|row| match row.first() {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(",")
    })
    .unwrap_or_default()
}

/// Rewrite a `CREATE TABLE` statement the way SQLite's shell prints it in
/// `.schema`/`.dump`: when the table name is quoted (`CREATE TABLE "…"` or
/// `CREATE TABLE '…'`), it inserts `IF NOT EXISTS` (SQLite's `printSchemaLine`).
/// Every other statement — a simple-named table, or an index/view/trigger — is
/// emitted verbatim.
fn schema_create_line(sql: &str) -> String {
    let after = sql.strip_prefix("CREATE TABLE ");
    match after {
        Some(rest) if rest.starts_with('"') || rest.starts_with('\'') => {
            format!("CREATE TABLE IF NOT EXISTS {rest}")
        }
        _ => sql.to_string(),
    }
}

/// Quote an identifier for the `INSERT INTO <name>` line only when SQLite would:
/// a name that is a plain identifier (letters, digits, `_`, not starting with a
/// digit) is emitted bare; anything else is `"`-quoted with internal quotes
/// doubled.
fn quote_ident_if_needed(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Render one value the way `.dump` does (distinct from list mode and from
/// `quote()`): NULL bare, integers as-is, a real as SQLite's `%!.20g`
/// (round-trip decimal), text single-quoted with `''` escaping, and a blob as
/// `X'<lowercase hex>'`.
fn dump_value_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("NULL"),
        Value::Integer(i) => out.push_str(&i.to_string()),
        Value::Real(r) if r.is_finite() => out.push_str(&graphitesql::util::fpdecode::format(
            *r,
            20,
            graphitesql::util::fpdecode::XType::Generic,
            true,
            false,
        )),
        // A non-finite real dumps as SQLite's sentinel literal.
        Value::Real(r) => out.push_str(if *r < 0.0 { "-9.0e+999" } else { "9.0e+999" }),
        Value::Text(s) => {
            out.push('\'');
            out.push_str(&s.replace('\'', "''"));
            out.push('\'');
        }
        Value::Blob(b) => {
            out.push_str("X'");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
        }
    }
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
    // A statement-level RETURNING only rides on INSERT/UPDATE/DELETE/REPLACE
    // (optionally WITH-prefixed). A RETURNING token anywhere else — most notably
    // inside a `CREATE TRIGGER … BEGIN … END` body — is not a statement-level
    // RETURNING; routing such a statement to `execute_returning` would wrongly
    // surface `execute_returning expects INSERT/UPDATE/DELETE` instead of letting
    // the executor reject the body construct with SQLite's own message.
    let lead = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_ascii_uppercase();
    if !matches!(
        lead.as_str(),
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "WITH"
    ) {
        return false;
    }
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
///
/// Exception: a handful of *row-returning* pragmas accept their argument in
/// either the `(arg)` or `=arg` form (`PRAGMA table_info=foo` is the same query
/// as `PRAGMA table_info(foo)`). Those `=arg` forms are getters, not setters, so
/// they must route to `query` and print their rows — never be mistaken for a
/// state-mutating setter just because they contain `=`.
fn is_pragma_setter(sql: &str) -> bool {
    let rest = sql.trim_start();
    let mut words = rest.split(|c: char| !c.is_ascii_alphabetic());
    let first = words.find(|w| !w.is_empty()).unwrap_or("");
    if !first.eq_ignore_ascii_case("PRAGMA") || !sql.contains('=') {
        return false;
    }
    // The pragma name: everything after PRAGMA up to `=`/`(`/`.`, last dotted part.
    let target = rest[first.len()..]
        .split(['=', '('])
        .next()
        .unwrap_or("")
        .trim();
    let name = target.rsplit('.').next().unwrap_or(target).trim();
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "table_info"
            | "table_xinfo"
            | "table_list"
            | "index_list"
            | "index_info"
            | "index_xinfo"
            | "foreign_key_list"
            | "foreign_key_check"
    )
}

/// A `PRAGMA [schema.]name = value` setter that SQLite still reports a result
/// row for: returns the equivalent getter (`PRAGMA [schema.]name`) to re-query
/// and print after the side effect has run. Currently only `journal_mode`, which
/// echoes the resulting journal mode (every other common setter is silent).
/// Any `schema.` qualifier is preserved so a per-database setter reads back from
/// the same database.
fn pragma_setter_result_query(sql: &str) -> Option<String> {
    let rest = sql.trim_start();
    if rest.len() < 6 || !rest[..6].eq_ignore_ascii_case("pragma") {
        return None;
    }
    // The target (possibly `schema.name`) is everything between PRAGMA and `=`.
    let target = rest[6..].split('=').next()?.trim();
    let name = target.rsplit('.').next().unwrap_or(target).trim();
    if name.eq_ignore_ascii_case("journal_mode") {
        Some(format!("PRAGMA {target}"))
    } else {
        None
    }
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
