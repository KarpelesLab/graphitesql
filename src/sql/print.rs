//! Rendering AST back to SQL text.
//!
//! Used by `ALTER TABLE` to regenerate the `CREATE TABLE`/`CREATE INDEX` text
//! stored in `sqlite_schema` after a schema change. Identifiers are always
//! double-quoted so the output re-parses unambiguously (valid SQL, if not always
//! the prettiest). It is a faithful-enough printer for the statements we store,
//! not a general formatter.

use crate::sql::ast::*;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Quote an identifier with double quotes, doubling any embedded quote.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Render a `CREATE TABLE` statement.
pub fn create_table(ct: &CreateTable) -> String {
    let mut parts: Vec<String> = ct.columns.iter().map(column_def).collect();
    for c in &ct.constraints {
        parts.push(table_constraint(c));
    }
    let mut s = String::from("CREATE TABLE ");
    s.push_str(&ident(&ct.name));
    s.push('(');
    s.push_str(&parts.join(", "));
    s.push(')');
    match (ct.without_rowid, ct.strict) {
        (true, true) => s.push_str(" WITHOUT ROWID, STRICT"),
        (true, false) => s.push_str(" WITHOUT ROWID"),
        (false, true) => s.push_str(" STRICT"),
        (false, false) => {}
    }
    s
}

/// Render a `CREATE INDEX` statement.
pub fn create_index(ci: &CreateIndex) -> String {
    let mut s = String::from("CREATE ");
    if ci.unique {
        s.push_str("UNIQUE ");
    }
    s.push_str("INDEX ");
    s.push_str(&ident(&ci.name));
    s.push_str(" ON ");
    s.push_str(&ident(&ci.table));
    s.push('(');
    let cols: Vec<String> = ci
        .columns
        .iter()
        .map(|t| {
            let mut c = expr(&t.expr);
            if t.descending {
                c.push_str(" DESC");
            }
            c
        })
        .collect();
    s.push_str(&cols.join(", "));
    s.push(')');
    if let Some(w) = &ci.where_clause {
        s.push_str(" WHERE ");
        s.push_str(&expr(w));
    }
    s
}

fn column_def(cd: &ColumnDef) -> String {
    let mut s = ident(&cd.name);
    if let Some(t) = &cd.type_name {
        s.push(' ');
        s.push_str(t);
    }
    for c in &cd.constraints {
        s.push(' ');
        s.push_str(&column_constraint(c));
    }
    s
}

fn column_constraint(c: &ColumnConstraint) -> String {
    match c {
        ColumnConstraint::PrimaryKey {
            descending,
            autoincrement,
        } => {
            let mut s = String::from("PRIMARY KEY");
            if *descending {
                s.push_str(" DESC");
            }
            if *autoincrement {
                s.push_str(" AUTOINCREMENT");
            }
            s
        }
        ColumnConstraint::NotNull => "NOT NULL".to_string(),
        ColumnConstraint::Unique => "UNIQUE".to_string(),
        ColumnConstraint::Default(e) => format!("DEFAULT {}", expr(e)),
        ColumnConstraint::Collate(n) => format!("COLLATE {n}"),
        ColumnConstraint::Check(e, _) => format!("CHECK ({})", expr(e)),
        ColumnConstraint::References(fk) => {
            format!("REFERENCES {}", foreign_key_target(fk))
        }
        ColumnConstraint::Generated { expr: e, stored } => {
            format!(
                "AS ({}) {}",
                expr(e),
                if *stored { "STORED" } else { "VIRTUAL" }
            )
        }
    }
}

/// Render a foreign key's `target(cols) [ON DELETE …] [ON UPDATE …]` tail.
fn foreign_key_target(fk: &ForeignKey) -> String {
    let mut s = ident(&fk.ref_table);
    if !fk.ref_columns.is_empty() {
        s.push_str(&format!(
            "({})",
            fk.ref_columns
                .iter()
                .map(|n| ident(n))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let action = |a: FkAction| match a {
        FkAction::NoAction => "NO ACTION",
        FkAction::Restrict => "RESTRICT",
        FkAction::Cascade => "CASCADE",
        FkAction::SetNull => "SET NULL",
        FkAction::SetDefault => "SET DEFAULT",
    };
    if fk.on_delete != FkAction::NoAction {
        s.push_str(&format!(" ON DELETE {}", action(fk.on_delete)));
    }
    if fk.on_update != FkAction::NoAction {
        s.push_str(&format!(" ON UPDATE {}", action(fk.on_update)));
    }
    s
}

fn table_constraint(c: &TableConstraint) -> String {
    let cols = |names: &[String]| -> String {
        names
            .iter()
            .map(|n| ident(n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match c {
        TableConstraint::PrimaryKey(names) => format!("PRIMARY KEY({})", cols(names)),
        TableConstraint::Unique(names) => format!("UNIQUE({})", cols(names)),
        TableConstraint::Check(e, _) => format!("CHECK ({})", expr(e)),
        TableConstraint::ForeignKey(fk) => format!(
            "FOREIGN KEY({}) REFERENCES {}",
            cols(&fk.columns),
            foreign_key_target(fk)
        ),
    }
}

/// Render an expression. Binary operations are fully parenthesized to preserve
/// precedence without tracking it.
pub fn expr(e: &Expr) -> String {
    match e {
        Expr::Literal(l) => literal(l),
        Expr::Parameter(_) => "?".to_string(),
        Expr::Column { table, column } => match table {
            Some(t) => format!("{}.{}", ident(t), ident(column)),
            None => ident(column),
        },
        Expr::Unary { op, expr: inner } => {
            let o = match op {
                UnaryOp::Negate => "-",
                UnaryOp::Identity => "+",
                UnaryOp::Not => "NOT ",
                UnaryOp::BitNot => "~",
            };
            format!("{o}{}", expr(inner))
        }
        Expr::Binary { op, left, right } => {
            format!("({} {} {})", expr(left), binary_op(*op), expr(right))
        }
        Expr::IsNull {
            expr: inner,
            negated,
        } => {
            format!(
                "{} IS{} NULL",
                expr(inner),
                if *negated { " NOT" } else { "" }
            )
        }
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => {
            let items: Vec<String> = list.iter().map(expr).collect();
            format!(
                "{}{} IN ({})",
                expr(inner),
                if *negated { " NOT" } else { "" },
                items.join(", ")
            )
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => format!(
            "{}{} BETWEEN {} AND {}",
            expr(inner),
            if *negated { " NOT" } else { "" },
            expr(low),
            expr(high)
        ),
        Expr::Collate {
            expr: inner,
            collation,
        } => {
            format!("{} COLLATE {}", expr(inner), collation)
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            let mut s = String::from("CASE");
            if let Some(o) = operand {
                s.push(' ');
                s.push_str(&expr(o));
            }
            for (w, t) in when_then {
                s.push_str(&format!(" WHEN {} THEN {}", expr(w), expr(t)));
            }
            if let Some(e) = else_result {
                s.push_str(&format!(" ELSE {}", expr(e)));
            }
            s.push_str(" END");
            s
        }
        Expr::Cast {
            expr: inner,
            type_name,
        } => format!("CAST({} AS {type_name})", expr(inner)),
        Expr::Function {
            name, args, star, ..
        } => {
            if *star {
                format!("{name}(*)")
            } else {
                let a: Vec<String> = args.iter().map(expr).collect();
                format!("{name}({})", a.join(", "))
            }
        }
        Expr::Paren(inner) => format!("({})", expr(inner)),
        Expr::RowValue(items) => {
            let parts: Vec<String> = items.iter().map(expr).collect();
            format!("({})", parts.join(", "))
        }
        // Subqueries are not expected in the schema text we regenerate; render a
        // placeholder so the printer stays total.
        Expr::Subquery(_) => "(SELECT ...)".to_string(),
        Expr::Exists { negated, .. } => {
            format!("{}EXISTS (SELECT ...)", if *negated { "NOT " } else { "" })
        }
        Expr::InSelect {
            expr: inner,
            negated,
            ..
        } => format!(
            "{}{} IN (SELECT ...)",
            expr(inner),
            if *negated { " NOT" } else { "" }
        ),
    }
}

fn binary_op(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Or => "OR",
        BinaryOp::And => "AND",
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        BinaryOp::Is => "IS",
        BinaryOp::IsNot => "IS NOT",
        BinaryOp::Like => "LIKE",
        BinaryOp::Glob => "GLOB",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Concat => "||",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::LShift => "<<",
        BinaryOp::RShift => ">>",
        BinaryOp::JsonExtract => "->",
        BinaryOp::JsonExtractText => "->>",
    }
}

fn literal(l: &Literal) -> String {
    match l {
        Literal::Null => "NULL".to_string(),
        Literal::Integer(i) => i.to_string(),
        Literal::Real(r) => {
            if *r == crate::util::float::trunc(*r) && r.is_finite() {
                format!("{r:.1}")
            } else {
                format!("{r}")
            }
        }
        Literal::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Literal::Blob(b) => {
            let mut s = String::from("x'");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s.push('\'');
            s
        }
        Literal::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse_one;

    fn roundtrip_table(sql: &str) -> CreateTable {
        match parse_one(sql).unwrap() {
            Statement::CreateTable(ct) => ct,
            _ => panic!(),
        }
    }

    #[test]
    fn create_table_reparses() {
        let ct = roundtrip_table(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT NOT NULL, c REAL DEFAULT 1.5)",
        );
        let printed = create_table(&ct);
        let reparsed = roundtrip_table(&printed);
        assert_eq!(ct, reparsed);
    }
}
