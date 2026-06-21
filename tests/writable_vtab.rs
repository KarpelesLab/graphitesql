//! Roadmap W1/W2: writable virtual tables. A module that overrides
//! `VTabModule::update` services INSERT/UPDATE/DELETE through `xUpdate`; the
//! default leaves the table read-only. `Connection::register_module` registers a
//! custom module. A `persistent()` module keeps its rows in a real `<vtab>_data`
//! backing table (W2), so they survive reopening the database file.

#![cfg(feature = "std")]

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use graphitesql::vtab::{
    IndexPlan, VTabChange, VTabCursor, VTabModule, VTabRow, VTabSchema, VTabStore,
};
use graphitesql::{Connection, Result, Value};

/// Shared `(rowid, values)` storage behind a [`MemModule`].
type MemRows = Rc<RefCell<Vec<(i64, Vec<Value>)>>>;

/// An in-memory writable module: `USING mem()` declares a `(k, v)` table whose
/// rows live in a shared `Vec`. Insert appends; delete/update mutate by rowid.
#[derive(Clone, Default)]
struct MemModule {
    rows: MemRows,
    next: Rc<Cell<i64>>,
}

struct MemCursor {
    rows: Vec<(i64, Vec<Value>)>,
    pos: usize,
}

struct MemRow {
    rowid: i64,
    values: Vec<Value>,
}

impl VTabRow for MemRow {
    fn column(&self, i: usize) -> Value {
        self.values.get(i).cloned().unwrap_or(Value::Null)
    }
    fn rowid(&self) -> i64 {
        self.rowid
    }
}

impl VTabCursor for MemCursor {
    type Row = MemRow;
    fn next(&mut self) -> Result<Option<MemRow>> {
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        let (rowid, values) = self.rows[self.pos].clone();
        self.pos += 1;
        Ok(Some(MemRow { rowid, values }))
    }
}

impl VTabModule for MemModule {
    type Cursor = MemCursor;

    fn connect(&self, _args: &[&str]) -> Result<VTabSchema> {
        Ok(VTabSchema::new(["k", "v"]))
    }

    fn open(&self, _args: &[&str], _plan: &IndexPlan) -> Result<MemCursor> {
        Ok(MemCursor {
            rows: self.rows.borrow().clone(),
            pos: 0,
        })
    }

    fn update(
        &self,
        _args: &[&str],
        change: VTabChange,
        _store: &mut dyn VTabStore,
    ) -> Result<i64> {
        match change {
            VTabChange::Insert { rowid, values } => {
                let id = rowid.unwrap_or_else(|| {
                    let n = self.next.get() + 1;
                    self.next.set(n);
                    n
                });
                self.rows.borrow_mut().push((id, values.to_vec()));
                Ok(id)
            }
            VTabChange::Delete { rowid } => {
                self.rows.borrow_mut().retain(|(r, _)| *r != rowid);
                Ok(rowid)
            }
            VTabChange::Update {
                rowid,
                new_rowid,
                values,
            } => {
                for (r, v) in self.rows.borrow_mut().iter_mut() {
                    if *r == rowid {
                        *r = new_rowid;
                        *v = values.to_vec();
                    }
                }
                Ok(new_rowid)
            }
        }
    }
}

#[test]
fn insert_into_a_writable_virtual_table() {
    let mut c = Connection::open_memory().unwrap();
    c.register_module("mem", MemModule::default()).unwrap();
    c.execute("CREATE VIRTUAL TABLE m USING mem()").unwrap();

    assert_eq!(c.execute("INSERT INTO m(k, v) VALUES ('a', 1)").unwrap(), 1);
    assert_eq!(
        c.execute("INSERT INTO m VALUES ('b', 2), ('c', 3)")
            .unwrap(),
        2
    );

    // The inserted rows read back through the module's scan cursor.
    let r = c.query("SELECT k, v FROM m ORDER BY k").unwrap();
    assert_eq!(
        r.rows,
        [
            vec![Value::Text("a".into()), Value::Integer(1)],
            vec![Value::Text("b".into()), Value::Integer(2)],
            vec![Value::Text("c".into()), Value::Integer(3)],
        ]
    );
    // A WHERE over the virtual table still filters correctly.
    assert_eq!(
        c.query("SELECT k FROM m WHERE v > 1 ORDER BY k")
            .unwrap()
            .rows,
        [vec![Value::Text("b".into())], vec![Value::Text("c".into())],]
    );
}

#[test]
fn insert_with_a_column_subset_defaults_the_rest_to_null() {
    let mut c = Connection::open_memory().unwrap();
    c.register_module("mem", MemModule::default()).unwrap();
    c.execute("CREATE VIRTUAL TABLE m USING mem()").unwrap();
    c.execute("INSERT INTO m(k) VALUES ('only-k')").unwrap();
    assert_eq!(
        c.query("SELECT k, v FROM m").unwrap().rows,
        [vec![Value::Text("only-k".into()), Value::Null]]
    );
}

#[test]
fn a_read_only_module_rejects_insert() {
    // The built-in `series` module does not override `update`, so it stays
    // read-only and an INSERT is rejected.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE s USING series(1, 5)")
        .unwrap();
    assert!(c.execute("INSERT INTO s VALUES (9)").is_err());
}

#[test]
fn delete_and_update_a_writable_virtual_table() {
    let mut c = Connection::open_memory().unwrap();
    c.register_module("mem", MemModule::default()).unwrap();
    c.execute("CREATE VIRTUAL TABLE m USING mem()").unwrap();
    c.execute("INSERT INTO m VALUES ('a',1),('b',2),('c',3),('d',4)")
        .unwrap();

    // DELETE with a WHERE removes only the matching rows.
    assert_eq!(c.execute("DELETE FROM m WHERE v > 2").unwrap(), 2);
    assert_eq!(
        c.query("SELECT k FROM m ORDER BY k").unwrap().rows,
        [vec![Value::Text("a".into())], vec![Value::Text("b".into())]]
    );

    // UPDATE evaluates each SET RHS against the original row.
    assert_eq!(
        c.execute("UPDATE m SET v = v + 10 WHERE k = 'a'").unwrap(),
        1
    );
    assert_eq!(
        c.query("SELECT k, v FROM m ORDER BY k").unwrap().rows,
        [
            vec![Value::Text("a".into()), Value::Integer(11)],
            vec![Value::Text("b".into()), Value::Integer(2)],
        ]
    );

    // DELETE with no WHERE clears the table.
    assert_eq!(c.execute("DELETE FROM m").unwrap(), 2);
    assert!(c.query("SELECT * FROM m").unwrap().rows.is_empty());
}

#[test]
fn insert_with_an_explicit_rowid() {
    let mut c = Connection::open_memory().unwrap();
    c.register_module("mem", MemModule::default()).unwrap();
    c.execute("CREATE VIRTUAL TABLE m USING mem()").unwrap();
    // An explicit `rowid` term sets the row's rowid (the module honors it).
    c.execute("INSERT INTO m(rowid, k, v) VALUES (100, 'x', 1)")
        .unwrap();
    c.execute("INSERT INTO m(k, v) VALUES ('y', 2)").unwrap();
    assert_eq!(
        c.query("SELECT rowid, k FROM m ORDER BY rowid")
            .unwrap()
            .rows,
        [
            vec![Value::Integer(1), Value::Text("y".into())],
            vec![Value::Integer(100), Value::Text("x".into())],
        ]
    );
}

// --- W2: a persistent module backed by a real <name>_data table ---

/// A persistent writable module: `persistent()` is true, so the engine creates a
/// `<vtab>_data` backing table, scans it for reads, and hands `update` a store.
struct PersistModule;

/// Never used (persistent reads scan the backing table, not the cursor).
struct EmptyCursor;
impl VTabCursor for EmptyCursor {
    type Row = MemRow;
    fn next(&mut self) -> Result<Option<MemRow>> {
        Ok(None)
    }
}

impl VTabModule for PersistModule {
    type Cursor = EmptyCursor;
    fn connect(&self, _args: &[&str]) -> Result<VTabSchema> {
        Ok(VTabSchema::new(["k", "v"]))
    }
    fn open(&self, _args: &[&str], _plan: &IndexPlan) -> Result<EmptyCursor> {
        Ok(EmptyCursor)
    }
    fn persistent(&self) -> bool {
        true
    }
    fn update(&self, _args: &[&str], change: VTabChange, store: &mut dyn VTabStore) -> Result<i64> {
        match change {
            VTabChange::Insert { rowid, values } => {
                let id = match rowid {
                    Some(r) => r,
                    None => store.rows()?.iter().map(|(id, _)| *id).max().unwrap_or(0) + 1,
                };
                store.put(id, values)?;
                Ok(id)
            }
            VTabChange::Delete { rowid } => {
                store.delete(rowid)?;
                Ok(rowid)
            }
            VTabChange::Update {
                rowid,
                new_rowid,
                values,
            } => {
                if new_rowid != rowid {
                    store.delete(rowid)?;
                }
                store.put(new_rowid, values)?;
                Ok(new_rowid)
            }
        }
    }
}

#[test]
fn persistent_module_stores_rows_in_a_real_backing_table() {
    let mut c = Connection::open_memory().unwrap();
    c.register_module("kv", PersistModule).unwrap();
    c.execute("CREATE VIRTUAL TABLE p USING kv()").unwrap();
    c.execute("INSERT INTO p VALUES ('a', 1), ('b', 2), ('c', 3)")
        .unwrap();

    // Reads come from the backing table.
    assert_eq!(
        c.query("SELECT k, v FROM p ORDER BY k").unwrap().rows,
        [
            vec![Value::Text("a".into()), Value::Integer(1)],
            vec![Value::Text("b".into()), Value::Integer(2)],
            vec![Value::Text("c".into()), Value::Integer(3)],
        ]
    );
    // The backing `p_data` table really exists and holds the rows.
    assert_eq!(
        c.query("SELECT k, v FROM p_data ORDER BY k").unwrap().rows,
        [
            vec![Value::Text("a".into()), Value::Integer(1)],
            vec![Value::Text("b".into()), Value::Integer(2)],
            vec![Value::Text("c".into()), Value::Integer(3)],
        ]
    );

    // DELETE and UPDATE persist through the store too.
    c.execute("DELETE FROM p WHERE v = 1").unwrap();
    c.execute("UPDATE p SET v = 20 WHERE k = 'b'").unwrap();
    assert_eq!(
        c.query("SELECT k, v FROM p ORDER BY k").unwrap().rows,
        [
            vec![Value::Text("b".into()), Value::Integer(20)],
            vec![Value::Text("c".into()), Value::Integer(3)],
        ]
    );
}

#[test]
fn persistent_vtab_survives_reopening_the_file() {
    let mut path = std::env::temp_dir();
    path.push(format!("graphitesql-w2-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    {
        let mut c = Connection::create(&path).unwrap();
        c.register_module("kv", PersistModule).unwrap();
        c.execute("CREATE VIRTUAL TABLE p USING kv()").unwrap();
        c.execute("INSERT INTO p VALUES ('x', 9)").unwrap();
    }
    // Reopen: the rows are still there (they were written to the real db file).
    {
        let mut c = Connection::open(&path).unwrap();
        c.register_module("kv", PersistModule).unwrap();
        assert_eq!(
            c.query("SELECT k, v FROM p").unwrap().rows,
            [vec![Value::Text("x".into()), Value::Integer(9)]]
        );
    }
    let _ = std::fs::remove_file(&path);
}
