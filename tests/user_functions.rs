//! Roadmap D4: user-defined scalar functions registered from Rust via
//! `Connection::register_function`, callable from SQL like any built-in.

#![cfg(feature = "std")]

use graphitesql::{Connection, Error, Result, Value};

fn val(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.remove(0).remove(0)
}

#[test]
fn scalar_udf_is_callable_from_sql() {
    let mut c = Connection::open_memory().unwrap();
    c.register_function("triple", |args: &[Value]| -> Result<Value> {
        match args {
            [Value::Integer(n)] => Ok(Value::Integer(n * 3)),
            _ => Err(Error::Error("triple() takes one integer".into())),
        }
    });
    assert_eq!(val(&c, "SELECT triple(7)"), Value::Integer(21));
    // Case-insensitive, and usable in expressions.
    assert_eq!(val(&c, "SELECT TRIPLE(2) + 1"), Value::Integer(7));
}

#[test]
fn udf_in_where_and_over_a_table() {
    let mut c = Connection::open_memory().unwrap();
    c.register_function("addone", |args: &[Value]| match args {
        [Value::Integer(n)] => Ok(Value::Integer(n + 1)),
        _ => Ok(Value::Null),
    });
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2),(3)").unwrap();
    assert_eq!(
        c.query("SELECT x FROM t WHERE addone(x) > 2 ORDER BY x")
            .unwrap()
            .rows,
        [vec![Value::Integer(2)], vec![Value::Integer(3)]]
    );
    assert_eq!(
        c.query("SELECT addone(x) FROM t ORDER BY x").unwrap().rows,
        [
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)],
        ]
    );
}

#[test]
fn multi_argument_and_text() {
    let mut c = Connection::open_memory().unwrap();
    c.register_function("join2", |args: &[Value]| match args {
        [Value::Text(a), Value::Text(b)] => Ok(Value::Text(format!("{a}-{b}").into())),
        _ => Err(Error::Error("join2(text, text)".into())),
    });
    assert_eq!(val(&c, "SELECT join2('a','b')"), Value::Text("a-b".into()));
}

#[test]
fn a_callback_error_propagates() {
    let mut c = Connection::open_memory().unwrap();
    c.register_function("strict1", |args: &[Value]| match args {
        [Value::Integer(_)] => Ok(Value::Integer(1)),
        _ => Err(Error::Error("strict1 wants one integer".into())),
    });
    assert!(c.query("SELECT strict1('x')").is_err());
    assert!(c.query("SELECT strict1(1, 2)").is_err());
}

#[test]
fn unknown_function_still_errors() {
    let c = Connection::open_memory().unwrap();
    assert!(c.query("SELECT no_such_fn(1)").is_err());
}

#[test]
fn builtin_takes_precedence() {
    let mut c = Connection::open_memory().unwrap();
    // Registering `upper` does not override the built-in.
    c.register_function("upper", |_args: &[Value]| Ok(Value::Text("UDF".into())));
    assert_eq!(val(&c, "SELECT upper('hi')"), Value::Text("HI".into()));
}

// --- user-defined aggregate functions ---

use graphitesql::AggregateFunction;

/// Aggregate that multiplies all its integer arguments.
struct Product {
    acc: i64,
}
impl AggregateFunction for Product {
    fn step(&mut self, args: &[Value]) -> Result<()> {
        match args {
            [Value::Integer(n)] => {
                self.acc *= n;
                Ok(())
            }
            _ => Err(Error::Error("product() takes one integer".into())),
        }
    }
    fn finalize(&mut self) -> Result<Value> {
        Ok(Value::Integer(self.acc))
    }
}

fn with_product() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.register_aggregate_function("product", || Box::new(Product { acc: 1 }));
    c
}

#[test]
fn aggregate_udf_over_whole_table() {
    let mut c = with_product();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (2),(3),(4)").unwrap();
    assert_eq!(val(&c, "SELECT product(x) FROM t"), Value::Integer(24));
}

#[test]
fn aggregate_udf_with_group_by() {
    let mut c = with_product();
    c.execute("CREATE TABLE t(g, x)").unwrap();
    c.execute("INSERT INTO t VALUES ('a',2),('a',5),('b',3),('b',4)")
        .unwrap();
    assert_eq!(
        c.query("SELECT g, product(x) FROM t GROUP BY g ORDER BY g")
            .unwrap()
            .rows,
        [
            vec![Value::Text("a".into()), Value::Integer(10)],
            vec![Value::Text("b".into()), Value::Integer(12)],
        ]
    );
    // Used inside a larger expression and with HAVING.
    assert_eq!(
        c.query("SELECT g FROM t GROUP BY g HAVING product(x) > 11 ORDER BY g")
            .unwrap()
            .rows,
        [vec![Value::Text("b".into())]]
    );
}

#[test]
fn aggregate_udf_honors_distinct() {
    let mut c = with_product();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (2),(2),(3)").unwrap();
    // DISTINCT folds the duplicate 2: 2*3 = 6, not 2*2*3 = 12.
    assert_eq!(
        val(&c, "SELECT product(DISTINCT x) FROM t"),
        Value::Integer(6)
    );
}
