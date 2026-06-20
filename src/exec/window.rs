//! Window-function support helpers: expression walking and frame math.
//!
//! The executor ([`super::Connection`]) computes each window function over the
//! post-`WHERE` rows, appends the results as synthetic columns, and rewrites the
//! projection to reference them. This module holds the dependency-free pieces:
//! collecting/replacing window expressions in a `SELECT`, and the default-frame
//! bounds within an ordered partition.

use crate::sql::ast::{Expr, ResultColumn, Select};
use alloc::vec::Vec;

/// Walk every sub-expression of `e` (pre-order), applying `f`. Does **not**
/// descend into nested `SELECT`s (subqueries handle their own windows).
pub fn visit(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Unary { expr, .. } => visit(expr, f),
        Expr::Binary { left, right, .. } => {
            visit(left, f);
            visit(right, f);
        }
        Expr::Function { args, .. } => {
            for a in args {
                visit(a, f);
            }
        }
        Expr::IsNull { expr, .. } => visit(expr, f),
        Expr::InList { expr, list, .. } => {
            visit(expr, f);
            for a in list {
                visit(a, f);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            visit(expr, f);
            visit(low, f);
            visit(high, f);
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            if let Some(o) = operand {
                visit(o, f);
            }
            for (w, t) in when_then {
                visit(w, f);
                visit(t, f);
            }
            if let Some(el) = else_result {
                visit(el, f);
            }
        }
        Expr::Cast { expr, .. } => visit(expr, f),
        Expr::Paren(inner) => visit(inner, f),
        Expr::InSelect { expr, .. } => visit(expr, f),
        _ => {}
    }
}

/// Replace any sub-expression equal to `target` with `repl` (pre-order; a
/// replaced node is not descended into). Does not descend into nested `SELECT`s.
fn replace_in(e: &mut Expr, target: &Expr, repl: &Expr) {
    if e == target {
        *e = repl.clone();
        return;
    }
    match e {
        Expr::Unary { expr, .. } => replace_in(expr, target, repl),
        Expr::Binary { left, right, .. } => {
            replace_in(left, target, repl);
            replace_in(right, target, repl);
        }
        Expr::Function { args, .. } => {
            for a in args {
                replace_in(a, target, repl);
            }
        }
        Expr::IsNull { expr, .. } => replace_in(expr, target, repl),
        Expr::InList { expr, list, .. } => {
            replace_in(expr, target, repl);
            for a in list {
                replace_in(a, target, repl);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            replace_in(expr, target, repl);
            replace_in(low, target, repl);
            replace_in(high, target, repl);
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            if let Some(o) = operand {
                replace_in(o, target, repl);
            }
            for (w, t) in when_then {
                replace_in(w, target, repl);
                replace_in(t, target, repl);
            }
            if let Some(el) = else_result {
                replace_in(el, target, repl);
            }
        }
        Expr::Cast { expr, .. } => replace_in(expr, target, repl),
        Expr::Paren(inner) => replace_in(inner, target, repl),
        Expr::InSelect { expr, .. } => replace_in(expr, target, repl),
        _ => {}
    }
}

/// Whether `e` is a window-function call (`f(…) OVER (…)`).
pub fn is_window(e: &Expr) -> bool {
    matches!(e, Expr::Function { over: Some(_), .. })
}

/// Collect the distinct window-function expressions used in `sel`'s projection
/// and `ORDER BY`, in first-seen order.
pub fn collect_window_exprs(sel: &Select) -> Vec<Expr> {
    let mut out: Vec<Expr> = Vec::new();
    let mut push = |e: &Expr| {
        if is_window(e) && !out.contains(e) {
            out.push(e.clone());
        }
    };
    for c in &sel.columns {
        if let ResultColumn::Expr { expr, .. } = c {
            visit(expr, &mut push);
        }
    }
    for t in &sel.order_by {
        visit(&t.expr, &mut push);
    }
    out
}

/// Whether `sel` uses any window function.
pub fn has_window(sel: &Select) -> bool {
    let mut found = false;
    let mut check = |e: &Expr| found |= is_window(e);
    for c in &sel.columns {
        if let ResultColumn::Expr { expr, .. } = c {
            visit(expr, &mut check);
        }
    }
    for t in &sel.order_by {
        visit(&t.expr, &mut check);
    }
    found
}

/// Replace every occurrence of `target` within `e` with `repl` (pre-order; a
/// replaced node is not descended into). Does not descend into nested `SELECT`s.
pub fn replace_expr(e: &mut Expr, target: &Expr, repl: &Expr) {
    replace_in(e, target, repl)
}

/// Replace every occurrence of `target` in `sel`'s projection and `ORDER BY`
/// with `repl`.
pub fn replace_window_expr(sel: &mut Select, target: &Expr, repl: &Expr) {
    for c in &mut sel.columns {
        if let ResultColumn::Expr { expr, .. } = c {
            replace_in(expr, target, repl);
        }
    }
    for t in &mut sel.order_by {
        replace_in(&mut t.expr, target, repl);
    }
}
