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
//! line. Other output modes (`.mode csv|column|line|tabs|quote|insert|json`),
//! result redirection (`.output`/`.once`), CSV import (`.import`), and a handful
//! of settings (`.separator`, `.nullvalue`, `.echo`, `.changes`) match the
//! `sqlite3` shell byte-for-byte.

use graphitesql::{Connection, QueryResult, Value};
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};

/// Output rendering mode, mirroring the `sqlite3` shell's `.mode`. Only the
/// modes the graphite shell supports are represented; an unknown mode is
/// rejected like SQLite. `tabs` is `List` with a tab column separator, so it has
/// no distinct variant here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `list`/`tabs` — columns joined by the column separator, rows by the row
    /// separator.
    List,
    /// `csv` — RFC-4180 quoting.
    Csv,
    /// `column` — left-justified fixed-width columns, two-space gaps.
    Column,
    /// `line` — one `name = value` per line, a blank line between rows.
    Line,
    /// `quote` — SQL literals separated by the column separator.
    Quote,
    /// `insert TABLE` — an `INSERT INTO TABLE VALUES(...)` per row.
    Insert,
    /// `json` — a JSON array of one object per row.
    Json,
    /// `markdown` — a GitHub-flavored Markdown table (always with a header).
    Markdown,
    /// `box` — a Unicode box-drawing table (always with a header).
    Box,
    /// `table` — an ASCII-art table (`+`/`-`/`|`, always with a header).
    Table,
    /// `html` — `<TR>`/`<TD>` table rows (header via `<TH>` when `.headers`).
    Html,
    /// `tcl` — each cell a Tcl/C-quoted string, separated by the column
    /// separator (a space by default); header row only when `.headers` is on.
    Tcl,
}

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

    let mut shell = Shell::new();
    shell.filename = path.clone();

    if !scripts.is_empty() {
        // One-shot mode: run each argument batch, exiting non-zero on the first
        // error (like the `sqlite3` shell).
        for sql in scripts {
            if let Err((e, stmt, _line)) = shell.run_sql_batch(&mut conn, sql, 1) {
                eprintln!("{}", render_cli_error(&stmt, &e));
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

/// Where a shell writes query output. `.output FILE`/`.once FILE` redirect here.
enum Sink {
    /// The default: the process's standard output.
    Stdout,
    /// A file opened by `.output FILE`/`.once FILE` (or `/dev/null` for
    /// `.output off`).
    File(File),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Stdout => io::stdout().write(buf),
            Sink::File(f) => f.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Stdout => io::stdout().flush(),
            Sink::File(f) => f.flush(),
        }
    }
}

struct Shell {
    /// Whether to print a header row before query results (`.headers on`, or a
    /// side effect of `.mode column`).
    headers: bool,
    /// Set once `.headers on|off` is used explicitly; suppresses the implicit
    /// header-on that `.mode column` would otherwise apply.
    header_set: bool,
    /// The active output mode (`.mode`). Defaults to `list`.
    mode: Mode,
    /// The column separator (`.separator COL` / mode default). `|` by default.
    col_sep: String,
    /// The row separator (`.separator … ROW` / mode default). `\n` by default,
    /// `\r\n` for CSV.
    row_sep: String,
    /// The string printed for a NULL value (`.nullvalue`). Empty by default.
    null_value: String,
    /// The target table name for `.mode insert TABLE` (default `table`).
    insert_table: String,
    /// Echo each SQL statement group before running it (`.echo on`).
    echo: bool,
    /// Print a `changes: N   total_changes: M` line after each SQL group
    /// (`.changes on`).
    count_changes: bool,
    /// Stop (exit non-zero) after the first error (`.bail on`). Off by default,
    /// matching SQLite: errors are reported and execution continues.
    bail: bool,
    /// Whether any statement error has occurred. In non-interactive (piped) mode
    /// SQLite exits non-zero if any error happened, even with `.bail off`.
    had_error: bool,
    /// Running total of rows changed by DML, for `.changes` `total_changes`.
    total_changes: u64,
    /// Rows changed by the most recent DML statement, for `.changes` `changes`.
    last_changes: u64,
    /// The current output sink; `.output`/`.once` redirect it.
    out: Sink,
    /// True while a `.once` redirect is active: the sink reverts to stdout after
    /// the next SQL group that produces output.
    once: bool,
    /// The SQLite mode name as last set (for `.show`), e.g. `list`, `box`.
    mode_name: String,
    /// The current output target's name (for `.show`): `stdout` or a file path.
    out_name: String,
    /// The database file path (for `.show`'s `filename` line).
    filename: String,
}

impl Shell {
    fn new() -> Self {
        Shell {
            headers: false,
            header_set: false,
            mode: Mode::List,
            col_sep: String::from("|"),
            row_sep: String::from("\n"),
            null_value: String::new(),
            insert_table: String::from("table"),
            echo: false,
            count_changes: false,
            bail: false,
            had_error: false,
            total_changes: 0,
            last_changes: 0,
            out: Sink::Stdout,
            once: false,
            mode_name: String::from("list"),
            out_name: String::from("stdout"),
            filename: String::from(":memory:"),
        }
    }

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
        // 1-based input line counter, and the line at which the current buffer began
        // — used to render `… near line N …` errors like the SQLite shell.
        let mut input_line = 0usize;
        let mut group_start_line = 1usize;

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
            input_line += 1;

            let trimmed = line.trim();
            // Dot-commands are only recognized at the start of a fresh buffer.
            if buffer.is_empty() && trimmed.starts_with('.') {
                // With `.echo on`, SQLite echoes every input line — dot-commands
                // included — before running it (the command turning echo on is not
                // itself echoed, since echo was still off when it was read).
                if self.echo {
                    let _ = writeln!(io::stdout(), "{trimmed}");
                }
                if self.dot_command(conn, trimmed) {
                    break;
                }
                continue;
            }

            // Remember the line the current statement group begins on.
            if buffer.is_empty() {
                group_start_line = input_line;
            }
            buffer.push_str(&line);
            // Execute once the accumulated input contains a complete statement.
            if buffer.trim_end().ends_with(';') {
                let sql = std::mem::take(&mut buffer);
                self.run_group(conn, &sql, group_start_line);
            }
        }
        // Non-interactive (piped) input: exit non-zero if any error occurred,
        // matching SQLite — even with `.bail off`, a failed statement makes the
        // shell exit with a failure code so scripts can detect it.
        if !interactive && self.had_error {
            std::process::exit(1);
        }
    }

    /// Process lines from `reader` exactly as the interactive loop does (minus the
    /// prompts): a line beginning a fresh buffer with `.` is a dot-command, others
    /// accumulate into an SQL statement that runs at each terminating `;`. Used by
    /// the `.read` command. Returns `true` if a `.quit`/`.exit` was reached.
    fn feed_reader(&mut self, conn: &mut Connection, reader: &mut impl BufRead) -> bool {
        let mut buffer = String::new();
        let mut input_line = 0usize;
        let mut group_start_line = 1usize;
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
            input_line += 1;
            let trimmed = line.trim();
            if buffer.is_empty() && trimmed.starts_with('.') {
                if self.echo {
                    let _ = writeln!(io::stdout(), "{trimmed}");
                }
                if self.dot_command(conn, trimmed) {
                    return true;
                }
                continue;
            }
            if buffer.is_empty() {
                group_start_line = input_line;
            }
            buffer.push_str(&line);
            if buffer.trim_end().ends_with(';') {
                let sql = std::mem::take(&mut buffer);
                self.run_group(conn, &sql, group_start_line);
            }
        }
        false
    }

    /// Run one accumulated input group (which may hold several `;`-separated
    /// statements): echo it if `.echo on`, execute it, then honor `.changes` and
    /// the pending `.once` redirect exactly as SQLite's per-line handling does.
    fn run_group(&mut self, conn: &mut Connection, sql: &str, start_line: usize) {
        if self.echo {
            // SQLite echoes the input group verbatim (trailing newline trimmed).
            let mut out = io::stdout();
            let _ = writeln!(out, "{}", sql.trim_end_matches('\n'));
        }
        if let Err((e, stmt, line)) = self.run_sql_batch(conn, sql, start_line) {
            // Match the SQLite shell's script/piped rendering: `Parse error near
            // line N: <msg>` (with a source line + caret for a locatable token) for a
            // prepare-time error, or `Runtime error near line N: <msg> (<code>)` for a
            // step-time one. (The one-shot `-arg` path uses the different
            // `Error: in prepare,`/`stepping,` wording; see `render_cli_error`.)
            eprintln!("{}", render_script_error(&stmt, &e, line));
            self.had_error = true;
            if self.bail {
                std::process::exit(1);
            }
        }
        if self.count_changes {
            let mut out = io::stdout();
            let _ = writeln!(
                out,
                "changes: {}   total_changes: {}",
                self.last_changes, self.total_changes
            );
        }
        // A `.once FILE` redirect covers exactly the next output-producing SQL
        // group, then reverts to stdout.
        if self.once {
            self.out = Sink::Stdout;
            self.once = false;
        }
    }

    /// Run one or more `;`-separated statements.
    #[allow(clippy::result_large_err)]
    fn run_sql_batch(
        &mut self,
        conn: &mut Connection,
        sql: &str,
        start_line: usize,
    ) -> Result<(), (graphitesql::Error, String, usize)> {
        // Track where each statement begins within the group so an error can name
        // the input line (`… near line N …`), like the SQLite shell. Statements are
        // located sequentially so a repeated statement text still maps to its own
        // occurrence; the line is the group's start plus the newlines that precede it.
        let mut search_from = 0usize;
        for stmt_raw in split_statements(sql) {
            let stmt = stmt_raw.trim();
            if stmt.is_empty() {
                continue;
            }
            let off = sql[search_from..]
                .find(stmt)
                .map(|p| search_from + p)
                .unwrap_or(search_from);
            search_from = off + stmt.len();
            let line = start_line + sql[..off].bytes().filter(|&b| b == b'\n').count();
            // On error, carry the failing statement text and its line so the caller
            // can render SQLite's `Parse`/`Runtime error near line N` message.
            if let Err(e) = self.run_one(conn, stmt) {
                return Err((e, stmt.to_string(), line));
            }
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
                        self.record_changes(conn.execute(sql)?);
                    }
                }
                Err(e) => return Err(e),
            }
        } else if has_returning(sql) {
            // INSERT/UPDATE/DELETE … RETURNING mutates *and* projects rows; run it
            // via execute_returning and print the projected rows.
            let result =
                conn.execute_returning(sql, &graphitesql::exec::eval::Params::default())?;
            self.record_changes(result.rows.len());
            self.print_result(&result);
        } else {
            match conn.execute(sql) {
                Ok(n) => {
                    // SQLite's `changes()`/`total_changes()` only count rows
                    // modified by INSERT/UPDATE/DELETE; DDL and other statements
                    // leave the counters untouched (the previous DML value
                    // persists), so only record for DML here.
                    if is_dml(sql) {
                        self.record_changes(n);
                    }
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

    /// Accumulate the row count of a DML statement into the `.changes` counters.
    fn record_changes(&mut self, n: usize) {
        self.last_changes = n as u64;
        self.total_changes += n as u64;
    }

    /// Print a query result in the active output mode. A result with zero rows
    /// prints nothing at all — not even a header — because the `sqlite3` CLI emits
    /// headers/framing from its per-row callback, which never fires for an empty
    /// result.
    fn print_result(&mut self, result: &QueryResult) {
        if result.rows.is_empty() {
            return;
        }
        match self.mode {
            Mode::List => self.print_list(result),
            Mode::Csv => self.print_csv(result),
            Mode::Column => self.print_column(result),
            Mode::Line => self.print_line(result),
            Mode::Quote => self.print_quote(result),
            Mode::Insert => self.print_insert(result),
            Mode::Json => self.print_json(result),
            Mode::Markdown => self.print_markdown(result),
            Mode::Box => self.print_boxed(result, BOX_CHARS),
            Mode::Table => self.print_boxed(result, TABLE_CHARS),
            Mode::Html => self.print_html(result),
            Mode::Tcl => self.print_tcl(result),
        }
    }

    /// `tcl` mode: each cell rendered as a Tcl/C-quoted string (`output_c_string`),
    /// cells joined by the column separator, one row per line. The header row is
    /// emitted only when `.headers` is on; an empty result set prints nothing.
    fn print_tcl(&mut self, result: &QueryResult) {
        if result.rows.is_empty() || result.columns.is_empty() {
            return;
        }
        let ncol = result.columns.len();
        let col_sep = self.col_sep.clone();
        let row_sep = self.row_sep.clone();
        let mut out: Vec<u8> = Vec::new();
        if self.headers {
            for (i, c) in result.columns.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(col_sep.as_bytes());
                }
                tcl_string(c.as_bytes(), &mut out);
            }
            out.extend_from_slice(row_sep.as_bytes());
        }
        for r in &result.rows {
            for i in 0..ncol {
                if i > 0 {
                    out.extend_from_slice(col_sep.as_bytes());
                }
                let mut bytes: Vec<u8> = Vec::new();
                match r.get(i) {
                    Some(Value::Null) | None => bytes.extend_from_slice(self.null_value.as_bytes()),
                    Some(v) => render_text_cell(v, &mut bytes),
                }
                tcl_string(&bytes, &mut out);
            }
            out.extend_from_slice(row_sep.as_bytes());
        }
        let _ = self.out.write_all(&out);
    }

    /// `html` mode: one `<TR>` per row, cells in `<TD>` (or `<TH>` for the
    /// header, emitted only when `.headers` is on). HTML-special characters are
    /// escaped. An empty result set prints nothing, matching SQLite.
    fn print_html(&mut self, result: &QueryResult) {
        if result.rows.is_empty() || result.columns.is_empty() {
            return;
        }
        let mut out = String::new();
        let row = |out: &mut String, cells: &[String], tag: &str| {
            out.push_str("<TR>");
            for (i, c) in cells.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push('<');
                out.push_str(tag);
                out.push('>');
                html_escape(c, out);
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
            out.push_str("\n</TR>\n");
        };
        if self.headers {
            row(&mut out, &result.columns, "TH");
        }
        let ncol = result.columns.len();
        for r in &result.rows {
            let cells: Vec<String> = (0..ncol)
                .map(|i| match r.get(i) {
                    Some(Value::Null) | None => self.null_value.clone(),
                    Some(v) => display_cell(v),
                })
                .collect();
            row(&mut out, &cells, "TD");
        }
        let _ = self.out.write_all(out.as_bytes());
    }

    /// Cells (as displayed) and per-column display widths (character counts),
    /// shared by the `markdown`/`box`/`table` renderers.
    fn boxed_cells(&self, result: &QueryResult) -> (Vec<Vec<String>>, Vec<usize>) {
        let ncol = result.columns.len();
        let cells: Vec<Vec<String>> = result
            .rows
            .iter()
            .map(|row| {
                (0..ncol)
                    .map(|i| match row.get(i) {
                        Some(Value::Null) | None => self.null_value.clone(),
                        Some(v) => escape_display_str(&display_cell(v)),
                    })
                    .collect()
            })
            .collect();
        let mut width: Vec<usize> = result.columns.iter().map(|c| c.chars().count()).collect();
        for row in &cells {
            for (i, c) in row.iter().enumerate() {
                width[i] = width[i].max(c.chars().count());
            }
        }
        (cells, width)
    }

    /// `markdown` mode: a GitHub-flavored Markdown table. Always includes a
    /// header row and its `|---|` separator (independent of `.headers`); an
    /// empty result set prints nothing, matching SQLite.
    fn print_markdown(&mut self, result: &QueryResult) {
        if result.rows.is_empty() || result.columns.is_empty() {
            return;
        }
        let (cells, width) = self.boxed_cells(result);
        let mut out = String::new();
        let row = |out: &mut String, cols: &[String], center: bool| {
            out.push('|');
            for (i, c) in cols.iter().enumerate() {
                out.push(' ');
                if center {
                    pad_center(c, width[i], out);
                } else {
                    pad_str(c, width[i], out);
                }
                out.push_str(" |");
            }
            out.push('\n');
        };
        row(&mut out, &result.columns, true);
        out.push('|');
        for &w in &width {
            for _ in 0..w + 2 {
                out.push('-');
            }
            out.push('|');
        }
        out.push('\n');
        for r in &cells {
            row(&mut out, r, false);
        }
        let _ = self.out.write_all(out.as_bytes());
    }

    /// `box`/`table` mode: a bordered table (Unicode box-drawing or ASCII art).
    /// Always includes a header (independent of `.headers`); an empty result set
    /// prints nothing, matching SQLite.
    fn print_boxed(&mut self, result: &QueryResult, c: BoxChars) {
        if result.rows.is_empty() || result.columns.is_empty() {
            return;
        }
        let (cells, width) = self.boxed_cells(result);
        let mut out = String::new();
        let border = |out: &mut String, l: char, mid: char, r: char| {
            out.push(l);
            for (i, &w) in width.iter().enumerate() {
                for _ in 0..w + 2 {
                    out.push(c.horiz);
                }
                out.push(if i == width.len() - 1 { r } else { mid });
            }
            out.push('\n');
        };
        let data_row = |out: &mut String, cols: &[String], center: bool| {
            out.push(c.vert);
            for (i, cell) in cols.iter().enumerate() {
                out.push(' ');
                if center {
                    pad_center(cell, width[i], out);
                } else {
                    pad_str(cell, width[i], out);
                }
                out.push(' ');
                out.push(c.vert);
            }
            out.push('\n');
        };
        border(&mut out, c.tl, c.tm, c.tr);
        data_row(&mut out, &result.columns, true);
        border(&mut out, c.ml, c.mm, c.mr);
        for row in &cells {
            data_row(&mut out, row, false);
        }
        border(&mut out, c.bl, c.bm, c.br);
        let _ = self.out.write_all(out.as_bytes());
    }

    /// List/tabs mode: header (optional) then one row per line, cells joined by
    /// the column separator, terminated by the row separator. A NULL renders as
    /// the `.nullvalue` string; TEXT/BLOB stop at the first NUL (C-string
    /// semantics), matching the `sqlite3` CLI.
    fn print_list(&mut self, result: &QueryResult) {
        // `list`/`tabs` apply SQLite's default control-character display escaping
        // (`^X`); `ascii` mode (same `Mode::List`, distinguished by its name) is
        // for machine parsing and sends bytes verbatim.
        let escape = self.mode_name != "ascii";
        let mut line: Vec<u8> = Vec::new();
        let emit = |line: &mut Vec<u8>, raw: &[u8]| {
            if escape {
                push_display_escaped(raw, line);
            } else {
                line.extend_from_slice(raw);
            }
        };
        if self.headers {
            for (i, c) in result.columns.iter().enumerate() {
                if i > 0 {
                    line.extend_from_slice(self.col_sep.as_bytes());
                }
                emit(&mut line, c.as_bytes());
            }
            line.extend_from_slice(self.row_sep.as_bytes());
        }
        for row in &result.rows {
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    line.extend_from_slice(self.col_sep.as_bytes());
                }
                let mut cell = Vec::new();
                render_list_cell(v, &self.null_value, &mut cell);
                emit(&mut line, &cell);
            }
            line.extend_from_slice(self.row_sep.as_bytes());
        }
        let _ = self.out.write_all(&line);
    }

    /// CSV mode: RFC-4180 quoting per field, `\r\n` row terminator (unless the
    /// row separator was overridden). NULL renders as the `.nullvalue` string,
    /// unquoted.
    fn print_csv(&mut self, result: &QueryResult) {
        let mut buf: Vec<u8> = Vec::new();
        if self.headers {
            for (i, c) in result.columns.iter().enumerate() {
                if i > 0 {
                    buf.extend_from_slice(self.col_sep.as_bytes());
                }
                csv_field(c.as_bytes(), &self.col_sep, &mut buf);
            }
            buf.extend_from_slice(self.row_sep.as_bytes());
        }
        for row in &result.rows {
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    buf.extend_from_slice(self.col_sep.as_bytes());
                }
                match v {
                    Value::Null => buf.extend_from_slice(self.null_value.as_bytes()),
                    _ => {
                        let mut cell = Vec::new();
                        render_text_cell(v, &mut cell);
                        csv_field(&cell, &self.col_sep, &mut buf);
                    }
                }
            }
            buf.extend_from_slice(self.row_sep.as_bytes());
        }
        let _ = self.out.write_all(&buf);
    }

    /// Column mode: compute each column's display width as the max of its header
    /// and cell widths (in characters), print a header + dashes row (when headers
    /// are on), then left-justify each cell padded to its column width, joined by
    /// two spaces. NULL uses the `.nullvalue` string.
    fn print_column(&mut self, result: &QueryResult) {
        let ncol = result.columns.len();
        if ncol == 0 {
            return;
        }
        // Cell text for every column of every row (as displayed, with control
        // characters caret-escaped so widths account for the expansion).
        let cells: Vec<Vec<String>> = result
            .rows
            .iter()
            .map(|row| {
                (0..ncol)
                    .map(|i| match row.get(i) {
                        Some(Value::Null) | None => self.null_value.clone(),
                        Some(v) => escape_display_str(&display_cell(v)),
                    })
                    .collect()
            })
            .collect();
        let mut width: Vec<usize> = result.columns.iter().map(|c| c.chars().count()).collect();
        for row in &cells {
            for (i, c) in row.iter().enumerate() {
                let w = c.chars().count();
                if w > width[i] {
                    width[i] = w;
                }
            }
        }
        let mut out: Vec<u8> = Vec::new();
        if self.headers {
            for (i, c) in result.columns.iter().enumerate() {
                pad_to(c, width[i], &mut out);
                out.extend_from_slice(if i == ncol - 1 { b"\n" } else { b"  " });
            }
            for (i, &w) in width.iter().enumerate().take(ncol) {
                out.extend(std::iter::repeat_n(b'-', w));
                out.extend_from_slice(if i == ncol - 1 { b"\n" } else { b"  " });
            }
        }
        for row in &cells {
            for (i, c) in row.iter().enumerate() {
                pad_to(c, width[i], &mut out);
                out.extend_from_slice(if i == ncol - 1 { b"\n" } else { b"  " });
            }
        }
        let _ = self.out.write_all(&out);
    }

    /// Line mode: for each row, one `<name> = <value>` per column (name
    /// right-justified to the widest column name), a blank line between rows.
    fn print_line(&mut self, result: &QueryResult) {
        // SQLite's line mode right-justifies each column name to a width that is
        // at least 5 (its floor) and at least the longest column name.
        let width = result
            .columns
            .iter()
            .map(|c| c.chars().count())
            .max()
            .unwrap_or(0)
            .max(5);
        let mut out: Vec<u8> = Vec::new();
        for (r, row) in result.rows.iter().enumerate() {
            if r > 0 {
                out.push(b'\n');
            }
            for (i, name) in result.columns.iter().enumerate() {
                let pad = width.saturating_sub(name.chars().count());
                out.extend(std::iter::repeat_n(b' ', pad));
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(b" = ");
                match row.get(i) {
                    Some(Value::Null) | None => {
                        push_display_escaped(self.null_value.as_bytes(), &mut out)
                    }
                    Some(v) => {
                        let mut cell = Vec::new();
                        render_text_cell(v, &mut cell);
                        push_display_escaped(&cell, &mut out);
                    }
                }
                out.push(b'\n');
            }
        }
        let _ = self.out.write_all(&out);
    }

    /// Quote mode: header (quoted) then rows of SQL literals joined by the column
    /// separator. NULL → `NULL`, text → `'...'`, integers/reals as-is (reals via
    /// `%!.20g`), blobs → `X'..'`.
    fn print_quote(&mut self, result: &QueryResult) {
        let mut out: Vec<u8> = Vec::new();
        if self.headers {
            for (i, c) in result.columns.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(self.col_sep.as_bytes());
                }
                let mut s = String::new();
                quote_text(c, &mut s);
                out.extend_from_slice(s.as_bytes());
            }
            out.extend_from_slice(self.row_sep.as_bytes());
        }
        for row in &result.rows {
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(self.col_sep.as_bytes());
                }
                let mut s = String::new();
                quote_value_with(v, real_inf, &mut s);
                out.extend_from_slice(s.as_bytes());
            }
            out.extend_from_slice(self.row_sep.as_bytes());
        }
        let _ = self.out.write_all(&out);
    }

    /// Insert mode: an `INSERT INTO <table>(cols...) VALUES(...);` per row. The
    /// column list is only emitted when headers are on (as SQLite does).
    fn print_insert(&mut self, result: &QueryResult) {
        let mut out: Vec<u8> = Vec::new();
        // Insert mode quotes the table and column names with SQLite's
        // keyword-aware rule (a bare identifier that is also a keyword — e.g.
        // `NULL` — is quoted); `ident_smart` implements exactly that.
        let table = graphitesql::sql::print::ident_smart(&self.insert_table);
        let collist = if self.headers {
            let cols = result
                .columns
                .iter()
                .map(|c| graphitesql::sql::print::ident_smart(c))
                .collect::<Vec<_>>()
                .join(",");
            format!("({cols})")
        } else {
            String::new()
        };
        for row in &result.rows {
            let mut line = format!("INSERT INTO {table}{collist} VALUES(");
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    line.push(',');
                }
                quote_value_with(v, real_sentinel, &mut line);
            }
            line.push_str(");\n");
            out.extend_from_slice(line.as_bytes());
        }
        let _ = self.out.write_all(&out);
    }

    /// JSON mode: a JSON array of objects, one object per row. NULL → `null`,
    /// integers as digits, reals via `%!.20g` (`±9.0e+999` for infinities), text
    /// and blobs via JSON string escaping.
    fn print_json(&mut self, result: &QueryResult) {
        let mut out: Vec<u8> = Vec::new();
        out.push(b'[');
        for (r, row) in result.rows.iter().enumerate() {
            if r == 0 {
                out.push(b'{');
            } else {
                out.extend_from_slice(b",\n{");
            }
            for (i, v) in row.iter().enumerate() {
                let name = result.columns.get(i).map(String::as_str).unwrap_or("");
                json_string(name.as_bytes(), &mut out);
                out.push(b':');
                json_value(v, &mut out);
                if i + 1 < row.len() {
                    out.push(b',');
                }
            }
            out.push(b'}');
        }
        out.extend_from_slice(b"]\n");
        let _ = self.out.write_all(&out);
    }

    /// Render an EXPLAIN QUERY PLAN result as SQLite's `QUERY PLAN` tree. The
    /// rows are `(id, parent, notused, detail)`; children link to their parent's
    /// `id` (top-level rows have parent 0). The last child of a node uses `` `-- ``
    /// (others `|--`), with `   ` / `|  ` continuation indent.
    fn print_eqp_tree(&mut self, result: &QueryResult) {
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
        let mut out: Vec<u8> = Vec::new();
        let _ = writeln!(out, "QUERY PLAN");
        render_eqp(&mut out, &nodes, 0, "");
        let _ = self.out.write_all(&out);
    }

    /// Handle a `.dot` command. Returns `true` if the shell should exit.
    fn dot_command(&mut self, conn: &mut Connection, line: &str) -> bool {
        let args = tokenize_dot(line);
        let cmd = args.first().map(String::as_str).unwrap_or("");
        let arg = args.get(1).map(String::as_str);
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
                Some(a) => {
                    self.headers = boolean_value(a);
                    self.header_set = true;
                }
                None => eprintln!("Usage: .headers on|off"),
            },
            ".mode" => self.set_mode(&args),
            ".separator" => match (args.get(1), args.get(2)) {
                (Some(col), row) => {
                    self.col_sep = col.clone();
                    if let Some(r) = row {
                        self.row_sep = r.clone();
                    }
                }
                _ => eprintln!("Usage: .separator COL ?ROW?"),
            },
            ".nullvalue" => match arg {
                Some(v) => self.null_value = v.to_string(),
                None => eprintln!("Usage: .nullvalue STRING"),
            },
            ".echo" => match arg {
                Some(a) => self.echo = boolean_value(a),
                None => eprintln!("Usage: .echo on|off"),
            },
            ".changes" => match arg {
                Some(a) => self.count_changes = boolean_value(a),
                None => eprintln!("Usage: .changes on|off"),
            },
            ".bail" => match arg {
                Some(a) => self.bail = boolean_value(a),
                None => eprintln!("Usage: .bail on|off"),
            },
            ".output" | ".once" => self.set_output(cmd, &args),
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
            ".import" => self.import(conn, &args),
            ".read" => {
                // The filename is the first token after `.read`.
                match arg {
                    None => eprintln!("Usage: .read FILE"),
                    Some(file) => match File::open(file) {
                        Ok(f) => {
                            let mut r = BufReader::new(f);
                            if self.feed_reader(conn, &mut r) {
                                return true; // a `.quit` inside the file exits
                            }
                        }
                        Err(_) => eprintln!("Error: cannot open \"{file}\""),
                    },
                }
            }
            ".print" => {
                // Echo the arguments, space-separated (SQLite's `.print`).
                println!("{}", args[1..].join(" "));
            }
            ".show" => self.show_settings(),
            ".backup" | ".save" => {
                // `.backup ?DB? FILE` / `.save FILE` — write a serialized copy of
                // the database to FILE. graphite serializes `main`; a leading DB
                // argument (always `main` here) is accepted and ignored.
                match args.get(1..).filter(|a| !a.is_empty()) {
                    None => eprintln!("Usage: .backup ?DB? FILE"),
                    Some(rest) => {
                        let file = rest.last().unwrap();
                        match conn.serialize() {
                            Ok(bytes) => {
                                if let Err(e) = std::fs::write(file, &bytes) {
                                    eprintln!("Error: cannot write \"{file}\": {e}");
                                }
                            }
                            Err(e) => eprintln!("Error: {e}"),
                        }
                    }
                }
            }
            other => eprintln!("Unknown command: {other}. Try \".help\"."),
        }
        false
    }

    /// Apply a `.mode` command. Mode names accept unambiguous prefixes (as
    /// SQLite's `.mode col` does). Sets the mode-specific column/row separators
    /// and, for `column`, enables headers unless `.headers` was set explicitly.
    fn set_mode(&mut self, args: &[String]) {
        // Positional args after `.mode`: the mode name, then (for insert) a table
        // name. `--`-options are accepted-and-ignored for compatibility.
        let mut positional = args[1..].iter().filter(|a| !a.starts_with('-'));
        let Some(name) = positional.next() else {
            // Bare `.mode` — leave the mode unchanged (SQLite reports it; we no-op
            // to avoid a spurious differential line).
            return;
        };
        let tabname = positional.next();
        let m = name.to_ascii_lowercase();
        let matches = |full: &str| !m.is_empty() && full.starts_with(m.as_str());
        if matches("list") {
            self.mode = Mode::List;
            self.mode_name = String::from("list");
            self.col_sep = String::from("|");
            self.row_sep = String::from("\n");
        } else if matches("csv") {
            self.mode = Mode::Csv;
            self.mode_name = String::from("csv");
            self.col_sep = String::from(",");
            self.row_sep = String::from("\r\n");
        } else if matches("columns") {
            self.mode = Mode::Column;
            self.mode_name = String::from("column");
            if !self.header_set {
                self.headers = true;
            }
            self.row_sep = String::from("\n");
        } else if matches("lines") {
            self.mode = Mode::Line;
            self.mode_name = String::from("line");
            self.row_sep = String::from("\n");
        } else if matches("tabs") {
            // `tabs` is list mode with a tab separator; SQLite's `.show` reports
            // it as `list` (there is no distinct internal tabs mode).
            self.mode = Mode::List;
            self.mode_name = String::from("list");
            self.col_sep = String::from("\t");
        } else if matches("quote") {
            self.mode = Mode::Quote;
            self.mode_name = String::from("quote");
            self.col_sep = String::from(",");
            self.row_sep = String::from("\n");
        } else if matches("insert") {
            self.mode = Mode::Insert;
            self.mode_name = String::from("insert");
            self.insert_table = tabname.cloned().unwrap_or_else(|| String::from("table"));
        } else if matches("json") {
            self.mode = Mode::Json;
            self.mode_name = String::from("json");
        } else if matches("markdown") {
            self.mode = Mode::Markdown;
            self.mode_name = String::from("markdown");
            self.row_sep = String::from("\n");
        } else if matches("box") {
            self.mode = Mode::Box;
            self.mode_name = String::from("box");
            self.row_sep = String::from("\n");
        } else if matches("table") {
            self.mode = Mode::Table;
            self.mode_name = String::from("table");
            self.row_sep = String::from("\n");
        } else if matches("html") {
            self.mode = Mode::Html;
            self.mode_name = String::from("html");
        } else if matches("tcl") {
            self.mode = Mode::Tcl;
            self.mode_name = String::from("tcl");
            self.col_sep = String::from(" ");
            self.row_sep = String::from("\n");
        } else if matches("ascii") {
            // ASCII mode is list mode with the unit/record separators.
            self.mode = Mode::List;
            self.mode_name = String::from("ascii");
            self.col_sep = String::from("\x1f");
            self.row_sep = String::from("\x1e");
        } else {
            eprintln!(
                "Error: mode should be one of: ascii box column csv html insert \
                 json line list markdown qbox quote table tabs tcl"
            );
        }
    }

    /// Apply `.output`/`.once`: redirect subsequent (or the next) SQL output to a
    /// file. `.output` with no file (or `off`/`stdout`) reverts to stdout. Only
    /// plain file targets are supported (no `-bom`/`-x`/pipe modes).
    fn set_output(&mut self, cmd: &str, args: &[String]) {
        let once = cmd == ".once";
        // First non-option positional argument is the file target.
        let target = args[1..].iter().find(|a| !a.starts_with('-'));
        match target.map(String::as_str) {
            None | Some("stdout") => {
                self.out = Sink::Stdout;
                self.out_name = String::from("stdout");
                self.once = false;
            }
            Some("off") => {
                // `.output off` discards output.
                match File::create("/dev/null") {
                    Ok(f) => self.out = Sink::File(f),
                    Err(_) => self.out = Sink::Stdout,
                }
                self.out_name = String::from("stdout");
                self.once = false;
            }
            Some(file) => match File::create(file) {
                Ok(f) => {
                    self.out = Sink::File(f);
                    self.out_name = file.to_string();
                    self.once = once;
                }
                Err(e) => {
                    eprintln!("Error: cannot open \"{file}\": {e}");
                    self.out = Sink::Stdout;
                    self.out_name = String::from("stdout");
                    self.once = false;
                }
            },
        }
    }

    /// Print the current shell settings, matching SQLite's `.show`. Output goes
    /// to the current sink (so a `.output` redirect captures it). Settings
    /// graphite does not model (`eqp`/`explain`/`stats`/`width`) are shown at
    /// SQLite's defaults.
    fn show_settings(&mut self) {
        // The column-family modes append the column wrap options to the name.
        let modestr = match self.mode_name.as_str() {
            m @ ("column" | "markdown" | "box" | "table") => {
                format!("{m} --wrap 60 --wordwrap off --noquote")
            }
            m => m.to_string(),
        };
        // Quote a separator/NULL value exactly as SQLite does (`output_c_string`).
        let q = |s: &str| {
            let mut v = Vec::new();
            tcl_string(s.as_bytes(), &mut v);
            String::from_utf8_lossy(&v).into_owned()
        };
        let onoff = |b: bool| if b { "on" } else { "off" };
        let lines = [
            format!("{:>12}: {}", "echo", onoff(self.echo)),
            format!("{:>12}: {}", "eqp", "off"),
            format!("{:>12}: {}", "explain", "auto"),
            format!("{:>12}: {}", "headers", onoff(self.headers)),
            format!("{:>12}: {}", "mode", modestr),
            format!("{:>12}: {}", "nullvalue", q(&self.null_value)),
            format!("{:>12}: {}", "output", self.out_name),
            format!("{:>12}: {}", "colseparator", q(&self.col_sep)),
            format!("{:>12}: {}", "rowseparator", q(&self.row_sep)),
            format!("{:>12}: {}", "stats", "off"),
            format!("{:>12}: {}", "width", ""),
            format!("{:>12}: {}", "filename", self.filename),
        ];
        let mut buf = String::new();
        for l in &lines {
            buf.push_str(l);
            buf.push('\n');
        }
        let _ = self.out.write_all(buf.as_bytes());
    }

    /// Implement `.import FILE TABLE`: read `FILE` field-by-field using the CSV
    /// reader (respecting the current `.separator`/mode), then insert rows into
    /// `TABLE`, creating it from the first row's field values as column names when
    /// it does not already exist (matching the `sqlite3` shell). Column-count
    /// mismatches emit the same `FILE:LINE: expected N columns…` warnings on
    /// stderr as SQLite.
    fn import(&mut self, conn: &mut Connection, args: &[String]) {
        // Positional args: FILE then TABLE. `--csv`/`--ascii`/`--skip N` alter
        // the separators; we support `--csv` and `--skip`.
        let mut file: Option<&str> = None;
        let mut table: Option<&str> = None;
        let mut col_sep = self.col_sep.clone();
        let mut row_sep = self.row_sep.clone();
        let mut skip = 0usize;
        let mut i = 1;
        while i < args.len() {
            let z = args[i].as_str();
            let opt = z.strip_prefix("--").or_else(|| z.strip_prefix('-'));
            match opt {
                Some("csv") => {
                    col_sep = String::from(",");
                    row_sep = String::from("\n");
                }
                Some("skip") if i + 1 < args.len() => {
                    skip = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
                Some(o) if z.starts_with('-') && !o.is_empty() => {
                    // Unsupported option; ignore for forward-compat.
                }
                _ => {
                    if file.is_none() {
                        file = Some(z);
                    } else if table.is_none() {
                        table = Some(z);
                    }
                }
            }
            i += 1;
        }
        let (Some(file), Some(table)) = (file, table) else {
            eprintln!(
                "ERROR: missing {} argument. Usage:\n.import FILE TABLE",
                if file.is_none() { "FILE" } else { "TABLE" }
            );
            return;
        };
        // When importing in CSV mode with the default `\r\n` output row
        // separator, SQLite permanently switches the row separator to `\n` (so
        // input and output separators need not be maintained separately). Mirror
        // that persistent mutation before deriving the single-byte reader
        // separators.
        if self.mode == Mode::Csv && row_sep == "\r\n" {
            self.row_sep = String::from("\n");
            row_sep = String::from("\n");
        }
        // The reader needs single-byte separators; strip any leading `\r` of a
        // `\r\n` row separator (SQLite also strips a trailing `\r` from each
        // field so CRLF input parses correctly).
        let col_sep = col_sep.bytes().next().unwrap_or(b',');
        let row_sep = row_sep.bytes().next_back().unwrap_or(b'\n');
        let data = match std::fs::read(file) {
            Ok(d) => d,
            Err(_) => {
                eprintln!("Error: cannot open \"{file}\"");
                return;
            }
        };
        let mut reader = CsvReader::new(&data, col_sep, row_sep);
        // Skip leading lines if requested.
        for _ in 0..skip {
            while reader.next_field().is_some() && reader.term == Term::Col {}
        }
        // Does the table exist? If not, create it from the first row's fields.
        let exists = conn
            .query(&format!(
                "SELECT count(*) FROM pragma_table_info('{}')",
                table.replace('\'', "''")
            ))
            .ok()
            .and_then(|r| match r.rows.first().and_then(|row| row.first()) {
                Some(Value::Integer(n)) => Some(*n > 0),
                _ => None,
            })
            .unwrap_or(false);
        if !exists {
            let mut cols: Vec<String> = Vec::new();
            while let Some(f) = reader.next_field() {
                cols.push(String::from_utf8_lossy(&f).into_owned());
                if reader.term != Term::Col {
                    break;
                }
            }
            if cols.is_empty() {
                eprintln!("{file}: empty file");
                return;
            }
            let coldefs = cols
                .iter()
                .map(|c| quote_ident_dq(c))
                .collect::<Vec<_>>()
                .join(",");
            let create = format!("CREATE TABLE {}({coldefs})", quote_ident_dq(table));
            if let Err(e) = conn.execute(&create) {
                eprintln!("{create} failed:\n{e}");
                return;
            }
        }
        // Determine the target table's column count.
        let ncol = conn
            .query(&format!(
                "SELECT count(*) FROM pragma_table_info('{}')",
                table.replace('\'', "''")
            ))
            .ok()
            .and_then(|r| match r.rows.first().and_then(|row| row.first()) {
                Some(Value::Integer(n)) => Some(*n as usize),
                _ => None,
            })
            .unwrap_or(0);
        if ncol == 0 {
            return;
        }
        let qtable = quote_ident_dq(table);
        loop {
            let start_line = reader.line;
            let mut fields: Vec<Option<Vec<u8>>> = Vec::with_capacity(ncol);
            let mut got = 0usize;
            let mut eof = false;
            while got < ncol {
                match reader.next_field() {
                    None => {
                        if got == 0 {
                            eof = true;
                        } else if got == ncol - 1 {
                            // RFC-4180: EOF may stand in for the final terminator.
                            fields.push(Some(Vec::new()));
                            got += 1;
                        }
                        break;
                    }
                    Some(f) => {
                        fields.push(Some(f));
                        got += 1;
                        if got < ncol && reader.term != Term::Col {
                            // Fewer columns than expected: NULL-fill the rest.
                            eprintln!(
                                "{file}:{start_line}: expected {ncol} columns but found {got} - filling the rest with NULL"
                            );
                            while fields.len() < ncol {
                                fields.push(None);
                            }
                            got = ncol;
                            break;
                        }
                    }
                }
            }
            if eof {
                break;
            }
            if fields.is_empty() {
                if reader.term == Term::Eof {
                    break;
                }
                continue;
            }
            // Extra columns: consume and count them for the warning.
            if reader.term == Term::Col {
                let mut extra = got;
                loop {
                    reader.next_field();
                    extra += 1;
                    if reader.term != Term::Col {
                        break;
                    }
                }
                eprintln!(
                    "{file}:{start_line}: expected {ncol} columns but found {extra} - extras ignored"
                );
            }
            if fields.len() >= ncol {
                let mut sql = format!("INSERT INTO {qtable} VALUES(");
                for (j, f) in fields.iter().take(ncol).enumerate() {
                    if j > 0 {
                        sql.push(',');
                    }
                    match f {
                        None => sql.push_str("NULL"),
                        Some(bytes) => {
                            sql.push('\'');
                            sql.push_str(&String::from_utf8_lossy(bytes).replace('\'', "''"));
                            sql.push('\'');
                        }
                    }
                }
                sql.push(')');
                if let Err(e) = conn.execute(&sql) {
                    eprintln!("{file}:{start_line}: INSERT failed: {e}");
                }
            }
            if reader.term == Term::Eof {
                break;
            }
        }
    }
}

fn print_help() {
    eprintln!(".help              Show this message");
    eprintln!(".tables [LIKE]     List table and view names");
    eprintln!(".indexes [LIKE]    List index names");
    eprintln!(".schema [TABLE]    Show CREATE statements");
    eprintln!(".databases         List attached databases");
    eprintln!(".dump              Dump the database as SQL text");
    eprintln!(".import FILE TABLE Import CSV data from FILE into TABLE");
    eprintln!(".read FILE         Execute SQL from FILE");
    eprintln!(".mode MODE ?TABLE? Set output mode (list csv column line tabs quote insert json)");
    eprintln!(".separator COL ?ROW?  Set column (and row) separators");
    eprintln!(".nullvalue STRING  Set the string printed for NULL values");
    eprintln!(".output ?FILE?     Redirect output to FILE (or back to stdout)");
    eprintln!(".once FILE         Redirect the next query's output to FILE");
    eprintln!(".echo on|off       Echo each SQL statement before running it");
    eprintln!(".changes on|off    Show the number of rows changed by each statement");
    eprintln!(".headers on|off    Toggle column headers (default off)");
    eprintln!(".quit / .exit      Exit the shell");
}

/// The terminator that ended a CSV field read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Term {
    /// The column separator (more fields follow on this row).
    Col,
    /// The row separator (end of this record).
    Row,
    /// End of input.
    Eof,
}

/// A minimal CSV/DSV field reader mirroring the `sqlite3` shell's
/// `csv_read_one_field`: unquoted fields end at the column or row separator (a
/// trailing `\r` is stripped before a row separator); a `"`-quoted field lets
/// `""` denote a literal `"` and may contain separators/newlines.
struct CsvReader<'a> {
    data: &'a [u8],
    pos: usize,
    col_sep: u8,
    row_sep: u8,
    /// The terminator of the most recently read field.
    term: Term,
    /// 1-based current line number (for warning messages).
    line: usize,
}

impl<'a> CsvReader<'a> {
    fn new(data: &'a [u8], col_sep: u8, row_sep: u8) -> Self {
        CsvReader {
            data,
            pos: 0,
            col_sep,
            row_sep,
            term: Term::Eof,
            line: 1,
        }
    }

    /// Read the next field, setting `self.term`. Returns `None` only at true EOF
    /// (no field to read).
    fn next_field(&mut self) -> Option<Vec<u8>> {
        if self.pos >= self.data.len() {
            self.term = Term::Eof;
            return None;
        }
        let mut out = Vec::new();
        let c = self.data[self.pos];
        if c == b'"' {
            self.pos += 1; // opening quote
            loop {
                if self.pos >= self.data.len() {
                    self.term = Term::Eof;
                    break;
                }
                let ch = self.data[self.pos];
                self.pos += 1;
                if ch == b'"' {
                    if self.pos < self.data.len() && self.data[self.pos] == b'"' {
                        out.push(b'"');
                        self.pos += 1;
                        continue;
                    }
                    // Closing quote: the next byte is the terminator.
                    if self.pos >= self.data.len() {
                        self.term = Term::Eof;
                    } else {
                        let t = self.data[self.pos];
                        if t == self.col_sep {
                            self.pos += 1;
                            self.term = Term::Col;
                        } else if t == self.row_sep {
                            self.pos += 1;
                            self.line += 1;
                            self.term = Term::Row;
                        } else if t == b'\r'
                            && self.pos + 1 < self.data.len()
                            && self.data[self.pos + 1] == self.row_sep
                        {
                            self.pos += 2;
                            self.line += 1;
                            self.term = Term::Row;
                        } else {
                            // Unexpected char after close quote: treat as row end.
                            self.pos += 1;
                            self.term = Term::Row;
                        }
                    }
                    break;
                }
                if ch == self.row_sep {
                    self.line += 1;
                }
                out.push(ch);
            }
        } else {
            loop {
                if self.pos >= self.data.len() {
                    self.term = Term::Eof;
                    break;
                }
                let ch = self.data[self.pos];
                if ch == self.col_sep {
                    self.pos += 1;
                    self.term = Term::Col;
                    break;
                }
                if ch == self.row_sep {
                    self.pos += 1;
                    self.line += 1;
                    self.term = Term::Row;
                    // Strip a trailing `\r` (CRLF handling).
                    if out.last() == Some(&b'\r') {
                        out.pop();
                    }
                    break;
                }
                out.push(ch);
                self.pos += 1;
            }
        }
        Some(out)
    }
}

/// Tokenize a `.dot` command line the way SQLite's shell does: split on
/// whitespace; a single-quoted token is literal until the closing `'`; a
/// double-quoted token honors backslash escapes (`\n`, `\t`, `\\`, `\"`, …).
fn tokenize_dot(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let mut tok = Vec::new();
        let delim = bytes[i];
        if delim == b'\'' || delim == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != delim {
                if delim == b'"' && bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    tok.push(unescape_byte(bytes[i]));
                } else {
                    tok.push(bytes[i]);
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // closing delim
            }
        } else {
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                tok.push(bytes[i]);
                i += 1;
            }
        }
        out.push(String::from_utf8_lossy(&tok).into_owned());
    }
    out
}

/// Map a backslash-escaped byte inside a double-quoted dot-command token to its
/// literal value (`\n` → newline, etc.). Unknown escapes pass through.
fn unescape_byte(c: u8) -> u8 {
    match c {
        b'a' => 0x07,
        b'b' => 0x08,
        b't' => b'\t',
        b'n' => b'\n',
        b'v' => 0x0b,
        b'f' => 0x0c,
        b'r' => b'\r',
        other => other,
    }
}

/// Parse a boolean argument the way SQLite's `booleanValue` does: `on`/`yes`/a
/// non-zero number → true; `off`/`no`/zero → false. Anything else warns and is
/// treated as false.
fn boolean_value(s: &str) -> bool {
    if let Ok(n) = s.parse::<i64>() {
        return n != 0;
    }
    match s.to_ascii_lowercase().as_str() {
        "on" | "yes" => true,
        "off" | "no" => false,
        _ => {
            eprintln!("ERROR: Not a boolean value: \"{s}\". Assuming \"no\".");
            false
        }
    }
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

/// Always double-quote an identifier (used where SQLite unconditionally quotes,
/// e.g. `.import`'s `CREATE TABLE`/`INSERT` targets).
fn quote_ident_dq(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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

/// The finite `%!.20g` round-trip rendering of a real.
fn real_g(r: f64) -> String {
    graphitesql::util::fpdecode::format(
        r,
        20,
        graphitesql::util::fpdecode::XType::Generic,
        true,
        false,
    )
}

/// A real for JSON/insert modes: `%!.20g` when finite, else SQLite's
/// `±9.0e+999` sentinel.
fn real_sentinel(r: f64) -> String {
    if r.is_finite() {
        real_g(r)
    } else if r < 0.0 {
        String::from("-9.0e+999")
    } else {
        String::from("9.0e+999")
    }
}

/// A real for `quote` mode: `%!.20g` when finite, else `Inf`/`-Inf` (what C's
/// `%!.20g` renders for an infinity).
fn real_inf(r: f64) -> String {
    if r.is_finite() {
        real_g(r)
    } else if r < 0.0 {
        String::from("-Inf")
    } else {
        String::from("Inf")
    }
}

/// Render a value as an SQL literal for a given real-rendering policy (quote mode
/// uses `Inf`/`-Inf` for non-finite reals; insert mode uses the `±9.0e+999`
/// sentinel). NULL → `NULL`, integer → digits, text → `'...'` (quotes doubled),
/// blob → `X'<hex>'`.
fn quote_value_with(v: &Value, real: fn(f64) -> String, out: &mut String) {
    match v {
        Value::Null => out.push_str("NULL"),
        Value::Integer(i) => out.push_str(&i.to_string()),
        Value::Real(r) => out.push_str(&real(*r)),
        Value::Text(s) => quote_text(s, out),
        Value::Blob(b) => {
            out.push_str("X'");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
        }
    }
}

/// Render a text string as an SQL string literal (`'...'`, quotes doubled).
fn quote_text(s: &str, out: &mut String) {
    out.push('\'');
    out.push_str(&s.replace('\'', "''"));
    out.push('\'');
}

/// The plain display string of a scalar value in tabular modes (column/etc.):
/// integers/text as-is, reals via the engine's canonical rendering, blobs as
/// their raw bytes (lossily as text). NULL is handled by the caller.
fn display_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Text(s) => {
            let b = s.as_bytes();
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        Value::Blob(b) => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
    }
}

/// Left-justify `s` into `out`, padded with spaces to `width` display
/// characters. If `s` is already at least `width` chars, no padding is added.
fn pad_to(s: &str, width: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(s.as_bytes());
    let n = s.chars().count();
    out.extend(std::iter::repeat_n(b' ', width.saturating_sub(n)));
}

/// Append `bytes` to `out`, applying SQLite's default control-character display
/// escaping (`SHELL_ESC_ASCII`): a control byte `c ≤ 0x1f` other than tab,
/// newline, or the `\r` of a `\r\n` is rendered as `^` followed by `c + 0x40`
/// (so `\x02` → `^B`); a NUL ends the (C-string) value. All other bytes,
/// including valid UTF-8 and `\x7f`, pass through verbatim.
fn push_display_escaped(bytes: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == 0 {
            break;
        }
        let is_crlf_cr = c == 0x0d && bytes.get(i + 1) == Some(&0x0a);
        if c <= 0x1f && c != b'\t' && c != b'\n' && !is_crlf_cr {
            out.push(b'^');
            out.push(0x40 + c);
        } else {
            out.push(c);
        }
        i += 1;
    }
}

/// Apply [`push_display_escaped`] to `s` and return the result as a `String`
/// (the escaping only emits ASCII `^X` and passes other bytes through, so the
/// result stays valid UTF-8). Used by the width-aligned display modes, which
/// must escape *before* computing column widths.
fn escape_display_str(s: &str) -> String {
    let mut out = Vec::new();
    push_display_escaped(s.as_bytes(), &mut out);
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Left-justify `s` to `width` display characters, appending to a `String`.
fn pad_str(s: &str, width: usize, out: &mut String) {
    out.push_str(s);
    let n = s.chars().count();
    for _ in 0..width.saturating_sub(n) {
        out.push(' ');
    }
}

/// Center `s` within `width` display characters (left-biased when the padding
/// is odd), appending to a `String`. Matches the header justification of
/// SQLite's `markdown`/`box`/`table` output.
fn pad_center(s: &str, width: usize, out: &mut String) {
    let n = s.chars().count();
    let pad = width.saturating_sub(n);
    let left = pad / 2;
    for _ in 0..left {
        out.push(' ');
    }
    out.push_str(s);
    for _ in 0..pad - left {
        out.push(' ');
    }
}

/// Append `z`'s bytes to `out` as a Tcl/C-quoted string (`"…"`), matching
/// SQLite's `output_c_string`: `"`/`\` and the `\t\n\r\f` escapes, other control
/// bytes and lone high bytes as `\ooo` (3-digit octal), and valid UTF-8
/// multibyte sequences passed through verbatim. (There is deliberately no `\b`
/// escape — a backspace becomes `\010`.)
fn tcl_string(z: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    let mut i = 0;
    while i < z.len() {
        let c = z[i];
        match c {
            b'"' => {
                out.extend_from_slice(b"\\\"");
                i += 1;
            }
            b'\\' => {
                out.extend_from_slice(b"\\\\");
                i += 1;
            }
            b'\t' => {
                out.extend_from_slice(b"\\t");
                i += 1;
            }
            b'\n' => {
                out.extend_from_slice(b"\\n");
                i += 1;
            }
            b'\r' => {
                out.extend_from_slice(b"\\r");
                i += 1;
            }
            0x0c => {
                out.extend_from_slice(b"\\f");
                i += 1;
            }
            0x20..=0x7e => {
                out.push(c);
                i += 1;
            }
            0x00..=0x1f => {
                out.extend_from_slice(format!("\\{c:03o}").as_bytes());
                i += 1;
            }
            _ => {
                // A byte >= 0x7f: pass a valid UTF-8 sequence through verbatim,
                // else escape the stray byte as octal.
                match utf8_seq_len(z, i) {
                    Some(len) => {
                        out.extend_from_slice(&z[i..i + len]);
                        i += len;
                    }
                    None => {
                        out.extend_from_slice(format!("\\{c:03o}").as_bytes());
                        i += 1;
                    }
                }
            }
        }
    }
    out.push(b'"');
}

/// Append `s` to `out`, escaping the HTML-special characters SQLite's `html`
/// mode escapes (`<`, `>`, `&`, `"`, `'`).
fn html_escape(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// The eleven border glyphs of a `box`/`table`-style bordered table.
#[derive(Clone, Copy)]
struct BoxChars {
    tl: char,
    tm: char,
    tr: char,
    ml: char,
    mm: char,
    mr: char,
    bl: char,
    bm: char,
    br: char,
    horiz: char,
    vert: char,
}

/// Unicode box-drawing glyphs (`.mode box`).
const BOX_CHARS: BoxChars = BoxChars {
    tl: '┌',
    tm: '┬',
    tr: '┐',
    ml: '├',
    mm: '┼',
    mr: '┤',
    bl: '└',
    bm: '┴',
    br: '┘',
    horiz: '─',
    vert: '│',
};

/// ASCII-art glyphs (`.mode table`).
const TABLE_CHARS: BoxChars = BoxChars {
    tl: '+',
    tm: '+',
    tr: '+',
    ml: '+',
    mm: '+',
    mr: '+',
    bl: '+',
    bm: '+',
    br: '+',
    horiz: '-',
    vert: '|',
};

/// Render a value's bytes for a CSV/line/list cell (not the NULL case — the
/// caller substitutes `.nullvalue`). TEXT/BLOB stop at the first NUL, as the
/// `sqlite3` CLI's C-string rendering does.
fn render_text_cell(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => {}
        Value::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Real(r) => {
            out.extend_from_slice(graphitesql::exec::eval::format_real(*r).as_bytes())
        }
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

/// Render a list-mode cell (NULL → the `.nullvalue` string, else `render_text_cell`).
fn render_list_cell(v: &Value, null_value: &str, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(null_value.as_bytes()),
        _ => render_text_cell(v, out),
    }
}

/// Append `field`'s CSV encoding to `out`, quoting per SQLite's rule: a field is
/// `"`-quoted (with internal `"` doubled) if it is empty, contains any byte that
/// needs quoting (control chars, space, `"`, `'`, or high bytes ≥ 0x80), or
/// contains the (possibly multi-byte) column separator.
fn csv_field(field: &[u8], col_sep: &str, out: &mut Vec<u8>) {
    let needs = field.is_empty()
        || field.iter().any(|&b| csv_needs_quote(b))
        || (!col_sep.is_empty() && contains(field, col_sep.as_bytes()));
    if needs {
        out.push(b'"');
        for &b in field {
            if b == b'"' {
                out.push(b'"');
            }
            out.push(b);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(field);
    }
}

/// Whether `b` forces CSV quoting, per SQLite's `needCsvQuote[]` table: every
/// byte `< 0x20`, space, `"`, `'`, and every byte `>= 0x7f`.
fn csv_needs_quote(b: u8) -> bool {
    b <= 0x20 || b == b'"' || b == b'\'' || b >= 0x7f
}

/// Whether `haystack` contains `needle` as a contiguous byte substring.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Append a JSON string literal (`"..."`) for `z`'s bytes, matching SQLite's
/// `output_json_string`: `"`/`\` and the standard `\b\f\n\r\t` escapes, control
/// bytes `<= 0x1f` and lone high bytes `>= 0x7f` as `\u00xx`. Valid UTF-8
/// multibyte sequences pass through unescaped.
fn json_string(z: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    let mut i = 0;
    while i < z.len() {
        let c = z[i];
        match c {
            b'"' => {
                out.extend_from_slice(b"\\\"");
                i += 1;
            }
            b'\\' => {
                out.extend_from_slice(b"\\\\");
                i += 1;
            }
            0x08 => {
                out.extend_from_slice(b"\\b");
                i += 1;
            }
            0x0c => {
                out.extend_from_slice(b"\\f");
                i += 1;
            }
            b'\n' => {
                out.extend_from_slice(b"\\n");
                i += 1;
            }
            b'\r' => {
                out.extend_from_slice(b"\\r");
                i += 1;
            }
            b'\t' => {
                out.extend_from_slice(b"\\t");
                i += 1;
            }
            0x00..=0x1f => {
                out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
                i += 1;
            }
            0x20..=0x7e => {
                out.push(c);
                i += 1;
            }
            _ => {
                // A byte >= 0x7f: if it starts a valid UTF-8 sequence, copy the
                // whole sequence verbatim; otherwise escape this byte as \u00xx.
                match utf8_seq_len(z, i) {
                    Some(len) => {
                        out.extend_from_slice(&z[i..i + len]);
                        i += len;
                    }
                    None => {
                        out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
                        i += 1;
                    }
                }
            }
        }
    }
    out.push(b'"');
}

/// If a valid UTF-8 multibyte sequence starts at `z[i]`, return its length
/// (2..=4); otherwise `None`. Used by `json_string` to pass real text through
/// unescaped while escaping stray high bytes (e.g. from blobs).
fn utf8_seq_len(z: &[u8], i: usize) -> Option<usize> {
    let c = z[i];
    let len = match c {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    if i + len > z.len() {
        return None;
    }
    if z[i + 1..i + len]
        .iter()
        .all(|&b| (0x80..=0xbf).contains(&b))
    {
        std::str::from_utf8(&z[i..i + len]).ok().map(|_| len)
    } else {
        None
    }
}

/// Render a value for JSON mode: NULL → `null`, integer → digits, real →
/// `%!.20g` (`±9.0e+999` for infinities), text/blob → JSON string.
fn json_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Real(r) => out.extend_from_slice(real_sentinel(*r).as_bytes()),
        Value::Text(s) => json_string(s.as_bytes(), out),
        Value::Blob(b) => json_string(b, out),
    }
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

/// Whether `sql` is a data-modification statement (INSERT/UPDATE/DELETE/REPLACE,
/// possibly `WITH`-prefixed) whose row count feeds SQLite's `changes()` /
/// `total_changes()`. DDL and everything else leave those counters unchanged.
fn is_dml(sql: &str) -> bool {
    let lead = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        lead.as_str(),
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "WITH"
    )
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
/// The raw error message SQLite would report (without graphite's `Display`
/// prefix). Matches the text SQLite puts after `Error: in prepare, ` /
/// `Error: stepping, `.
fn raw_error_message(e: &graphitesql::Error) -> String {
    use graphitesql::Error as E;
    match e {
        E::Error(m)
        | E::ErrorAt(m, _)
        | E::Corrupt(m)
        | E::Io(m)
        | E::CantOpen(m)
        | E::Constraint(m)
        | E::Parse(m)
        | E::ParseAt(m, _) => m.clone(),
        E::Busy => String::from("database is locked"),
        E::Unsupported(m) => String::from(*m),
        // `Error` is non-exhaustive; fall back to Display for any future variant.
        other => format!("{other}"),
    }
}

/// Whether an error occurs at *prepare* time (SQLite renders `in prepare,` and,
/// when it has a source position, a caret) rather than at *step* time
/// (`stepping,`, no caret). Syntax errors and every name/type-resolution / DDL
/// validation error are prepare-time; constraint violations and the handful of
/// run-time faults are step-time. The step set is enumerated (it is small and
/// stable) and everything else defaults to prepare, so a not-yet-listed prepare
/// error is classified correctly rather than mislabelled `stepping`.
fn is_prepare_error(e: &graphitesql::Error, msg: &str) -> bool {
    use graphitesql::Error as E;
    match e {
        E::Parse(_) | E::ParseAt(..) => true,
        // A constraint violation, a lock/busy, a corrupt/IO/open failure is a
        // run-time (step) error.
        E::Constraint(_) | E::Busy | E::Corrupt(_) | E::Io(_) | E::CantOpen(_) => false,
        // A generic error is prepare-time only when it is one of SQLite's finite
        // name-resolution / schema / DDL-validation diagnostics. Everything else —
        // vtab/FTS5 faults, the ALTER re-validation `error in …`, `SQL logic error`,
        // and other run-time messages — defaults to step (they are open-ended, so
        // enumerating the prepare set is the reliable direction).
        E::Error(_) | E::ErrorAt(..) => {
            const PREPARE_PREFIXES: &[&str] = &[
                "no such column",
                "no such table",
                "no such function",
                "no such collation sequence",
                "no such index",
                "no such view",
                "no such trigger",
                "no such module",
                "ambiguous column name",
                "misuse of aggregate function",
                "misuse of window function",
                "wrong number of arguments to function",
                "too many arguments on",
                "duplicate column name",
                "row value misused",
                "aggregate functions are not allowed",
                "sub-select returns",
                "table ", // "table X has N columns…" / "table X already exists"
                "there is already ",
                "unknown table option",
                "unknown database",
                "1st ORDER BY term",
                "ORDER BY term out of range",
                "default value of column",
                "object name reserved for internal use",
                "foreign key on ",
                "number of columns in foreign key",
                "parameters prohibited",
                "second argument to nth_value",
                "no query solution",
                "missing datatype for",
                "AUTOINCREMENT is only allowed",
                "cannot use DEFAULT on a generated column",
                "cannot use window functions in recursive",
                "generated columns cannot",
                "must have at least one non-generated column",
                "Cannot add a UNIQUE",
                "Cannot add a PRIMARY KEY",
                "unsupported frame specification",
                "cannot create",
                "conflicting ON CONFLICT",
                "the NATURAL keyword",
                "a JOIN clause is required",
                "USING clause",
                "cannot have more than one primary key",
                "has more than one primary key",
                "PRIMARY KEY missing on table",
                "the \".\" operator prohibited",
                "cannot modify ",
                "ON CONFLICT clause does not match",
                "RANGE with offset",
                "GROUPS with offset",
            ];
            // Patterns that carry a leading ordinal / function name, so a prefix
            // match won't do (`2nd ORDER BY term out of range`, `abs() may not be
            // used as a window function`).
            const PREPARE_CONTAINS: &[&str] = &[
                "ORDER BY term out of range",
                "GROUP BY term out of range",
                "may not be used as a window function",
            ];
            PREPARE_PREFIXES.iter().any(|p| msg.starts_with(p))
                || PREPARE_CONTAINS.iter().any(|p| msg.contains(p))
                || msg.ends_with(" already exists")
        }
        _ => false,
    }
}

/// The offending identifier a prepare-error message names, whose first source
/// occurrence SQLite's caret points at — but only for the specific error classes
/// SQLite actually renders a caret for. Many prepare errors (`no such table`,
/// `no such collation sequence`, `no such index/view/trigger`, the column-count and
/// row-value errors, …) carry no source position and show no caret, so they return
/// `None` here.
fn error_offending_token(msg: &str) -> Option<&str> {
    if let Some(rest) = msg
        .strip_prefix("near \"")
        .or_else(|| msg.strip_prefix("unrecognized token: \""))
    {
        return rest.split('"').next().filter(|t| !t.is_empty());
    }
    for pre in [
        "misuse of aggregate function ",
        "misuse of window function ",
        "wrong number of arguments to function ",
    ] {
        if let Some(rest) = msg.strip_prefix(pre) {
            return rest.split('(').next().filter(|t| !t.is_empty());
        }
    }
    // Among the colon-delimited resolution errors, SQLite carets only these three
    // (the identifier is in the current statement's scope). The `no such column`
    // form may carry a trailing ` - should this be a string literal…` hint.
    for pre in [
        "no such column: ",
        "no such function: ",
        "ambiguous column name: ",
    ] {
        if let Some(rest) = msg.strip_prefix(pre) {
            let tok = rest.split(" - ").next().unwrap_or(rest);
            return (!tok.is_empty() && !tok.contains(' ')).then_some(tok);
        }
    }
    // `<kind> <name> already exists` carets the object name — but only for a table,
    // view, or trigger; the `index` form and the `there is already …` variant carry
    // no source position (no caret).
    for kind in ["table ", "view ", "trigger "] {
        if let Some(rest) = msg.strip_prefix(kind)
            && let Some(name) = rest.strip_suffix(" already exists")
        {
            return (!name.is_empty() && !name.contains(' ')).then_some(name);
        }
    }
    None
}

/// Byte offset of `token`'s first occurrence in `sql` that is not inside a
/// single-quoted string literal or a comment (SQLite's caret points at the code
/// occurrence, never one inside a string). A double-quoted `"ident"` is *not*
/// skipped — it is a valid identifier the token may itself be.
fn locate_token(sql: &str, token: &str) -> Option<usize> {
    // A token that itself begins with `'` is an unterminated / malformed string
    // literal (`unrecognized token: "'abc"`); the string-skipping below would hide
    // it, so search plainly.
    if token.starts_with('\'') {
        return sql.find(token);
    }
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                // Skip a single-quoted string (doubled '' is an escaped quote).
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                if sql[i..].starts_with(token) {
                    return Some(i);
                }
                i += 1;
            }
        }
    }
    None
}

/// Render an error the way the SQLite CLI does for the failed statement `sql`:
/// `Error: in prepare, <msg>` (with a source line and `^--- error here` caret when
/// the message names a locatable token) for a prepare-time error, or
/// `Error: stepping, <msg>[ (<code>)]` for a step-time error (the extended result
/// code is shown when it is not the generic `SQLITE_ERROR`).
/// Build the two-line `  <source>\n<caret>` block the SQLite shell prints under an
/// error, given the offending statement `src` and the byte `off`set of the error
/// token within it. Mirrors `shell_error_context`: a far-right token is kept visible
/// by sliding the displayed line's start forward while the offset exceeds 50 (on
/// char boundaries), the line is capped at 78 bytes, and the caret flips from the
/// right-pointing form (`^--- error here`) to the left-pointing one
/// (`error here ---^`) once the offset reaches 25.
fn caret_block(src: &str, off: usize) -> String {
    // Slide the window start forward until the offset is within 50 of it.
    let mut start = 0usize;
    let mut ioff = off;
    while ioff > 50 {
        let ch_len = src[start..].chars().next().map_or(1, |c| c.len_utf8());
        start += ch_len;
        ioff -= 1;
    }
    // Displayed line: from `start`, capped at 78 bytes on a char boundary, and with
    // any trailing newline removed.
    let tail = src[start..].trim_end_matches(['\n', '\r']);
    let mut end = tail.len().min(78);
    while end < tail.len() && !tail.is_char_boundary(end) {
        end -= 1;
    }
    let shown = &tail[..end];
    let caret = if ioff < 25 {
        format!("{}^--- error here", " ".repeat(2 + ioff))
    } else {
        format!("{}error here ---^", " ".repeat(2 + ioff - 14))
    };
    format!("  {shown}\n{caret}")
}

fn render_cli_error(sql: &str, e: &graphitesql::Error) -> String {
    let msg = raw_error_message(e);
    if is_prepare_error(e, &msg) {
        // Prefer the parser's exact byte offset (a syntax error carries it, like
        // `sqlite3_error_offset`) so the caret is right even for a repeated token
        // (`===`); fall back to locating the message's token by text for errors
        // without an offset (resolution errors, etc.).
        let off = e
            .parse_offset()
            .filter(|&o| o <= sql.len())
            .or_else(|| error_offending_token(&msg).and_then(|t| locate_token(sql, t)));
        if let Some(off) = off {
            return format!("Error: in prepare, {msg}\n{}", caret_block(sql, off));
        }
        return format!("Error: in prepare, {msg}");
    }
    // Step-time: append the extended result code unless it is SQLITE_ERROR (1).
    let code = e.code();
    if code != 1 {
        format!("Error: stepping, {msg} ({code})")
    } else {
        format!("Error: stepping, {msg}")
    }
}

/// Collapse every run of whitespace (including newlines) in `s` to a single space,
/// trimming the ends — the SQLite shell renders a multi-line offending statement as
/// one space-joined line under the error caret.
fn collapse_ws(s: &str) -> String {
    let mut out = String::new();
    let mut prev_ws = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Render an error the way the SQLite CLI does when running a *script* (piped stdin,
/// `.read`, or interactive): `Parse error near line N: <msg>` — with the offending
/// statement (whitespace-collapsed) and a `^--- error here` caret when the message
/// names a locatable token — for a prepare-time error, or `Runtime error near line
/// N: <msg>[ (<code>)]` for a step-time error. `line` is the 1-based input line the
/// failing statement begins on. This is the script analogue of [`render_cli_error`]
/// (which uses the one-shot `-arg` wording).
fn render_script_error(sql: &str, e: &graphitesql::Error, line: usize) -> String {
    let msg = raw_error_message(e);
    let flat = collapse_ws(sql);
    if is_prepare_error(e, &msg) {
        // The parser's byte offset is into the original `sql`; it aligns with the
        // whitespace-collapsed `flat` only when no collapse happened (a single-line
        // statement, which the shell trims). Use it then (exact even for a repeated
        // token); otherwise fall back to text-locating the token in `flat`.
        let off = e
            .parse_offset()
            .filter(|_| flat == sql)
            .or_else(|| error_offending_token(&msg).and_then(|t| locate_token(&flat, t)));
        if let Some(off) = off {
            return format!(
                "Parse error near line {line}: {msg}\n{}",
                caret_block(&flat, off)
            );
        }
        return format!("Parse error near line {line}: {msg}");
    }
    let code = e.code();
    if code != 1 {
        format!("Runtime error near line {line}: {msg} ({code})")
    } else {
        format!("Runtime error near line {line}: {msg}")
    }
}

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
    // Setters whose `= value` form itself echoes the resulting value (SQLite prints
    // it), as opposed to the silent setters (`synchronous`, `temp_store`, …). Each
    // stores the value, so re-querying the getter reproduces the echoed line.
    if matches!(
        name.to_ascii_lowercase().as_str(),
        "journal_mode"
            | "busy_timeout"
            | "threads"
            | "secure_delete"
            | "soft_heap_limit"
            | "wal_autocheckpoint"
            | "journal_size_limit"
            | "analysis_limit"
    ) {
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
