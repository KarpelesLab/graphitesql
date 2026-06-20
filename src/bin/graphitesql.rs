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

    // First argument (if any) is the database path; the rest, if present, is a
    // one-shot SQL script to run and exit.
    let (path, script) = match args.split_first() {
        None => (String::from(":memory:"), None),
        Some((db, rest)) => {
            let script = if rest.is_empty() {
                None
            } else {
                Some(rest.join(" "))
            };
            (db.clone(), script)
        }
    };

    let mut conn = match open(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: unable to open {path:?}: {e}");
            std::process::exit(1);
        }
    };

    let mut shell = Shell { headers: false };

    if let Some(sql) = script {
        // One-shot mode: run the script, exit non-zero on the first error.
        if let Err(e) = shell.run_sql_batch(&mut conn, &sql) {
            eprintln!("Error: {e}");
            std::process::exit(1);
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
        if returns_rows(sql) {
            let result = conn.query(sql)?;
            self.print_result(&result);
        } else {
            conn.execute(sql)?;
        }
        Ok(())
    }

    fn print_result(&self, result: &QueryResult) {
        let out = io::stdout();
        let mut out = out.lock();
        if self.headers {
            let _ = writeln!(out, "{}", result.columns.join("|"));
        }
        for row in &result.rows {
            let cells: Vec<String> = row.iter().map(render_value).collect();
            let _ = writeln!(out, "{}", cells.join("|"));
        }
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

/// Render a value the way the `sqlite3` shell does in list mode (NULL prints as
/// the empty string).
fn render_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        // Use the engine's canonical real rendering (15 significant digits with
        // `%g`-style fixed/exponential switching, `Inf`/`-Inf`, `0.0` for zero) so
        // the shell matches the `sqlite3` CLI for large/small/special magnitudes —
        // e.g. `1e18` -> `1.0e+18`, `1e400` -> `Inf`, `-0.0` -> `0.0`.
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => {
            let mut s = String::with_capacity(b.len() * 2);
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s
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
    matches!(word.as_str(), "SELECT" | "PRAGMA" | "WITH" | "VALUES")
}

/// Split a batch into statements on `;`, respecting single-quoted strings so a
/// `;` inside a literal does not split it. (Good enough for a shell; the engine
/// re-parses each piece.)
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_str = !in_str;
                // A doubled '' inside a string is an escaped quote.
                if !in_str && chars.peek() == Some(&'\'') {
                    cur.push('\'');
                    cur.push(chars.next().unwrap());
                    in_str = true;
                    continue;
                }
                cur.push(c);
            }
            ';' if !in_str => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}
