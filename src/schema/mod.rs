//! The schema catalog: graphitesql's view of what tables and indexes exist.
//!
//! SQLite stores its own schema as ordinary rows in a table b-tree rooted at
//! **page 1**, called `sqlite_schema` (historically `sqlite_master`). Each row
//! has five columns:
//!
//! | # | column     | meaning |
//! |---|------------|---------|
//! | 0 | `type`     | `"table"`, `"index"`, `"view"`, or `"trigger"` |
//! | 1 | `name`     | object name |
//! | 2 | `tbl_name` | table the object is attached to |
//! | 3 | `rootpage` | b-tree root page (0 for views/triggers) |
//! | 4 | `sql`      | the `CREATE` statement text (NULL for auto indexes) |
//!
//! Reading this is the first thing any connection does: it bootstraps from the
//! header (page 1) to a catalog the query layer can resolve names against.

use crate::btree::TableCursor;
use crate::error::{Error, Result};
use crate::format::record::decode_record;
use crate::pager::PageSource;
use crate::value::Value;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The page number of the `sqlite_schema` table b-tree root.
pub const SCHEMA_ROOT_PAGE: u32 = 1;

/// The kind of a schema object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    /// A table.
    Table,
    /// An index.
    Index,
    /// A view.
    View,
    /// A trigger.
    Trigger,
}

impl ObjectType {
    fn parse(s: &str) -> Result<ObjectType> {
        Ok(match s {
            "table" => ObjectType::Table,
            "index" => ObjectType::Index,
            "view" => ObjectType::View,
            "trigger" => ObjectType::Trigger,
            other => {
                return Err(Error::Corrupt(format!(
                    "unknown schema object type {other:?}"
                )))
            }
        })
    }
}

/// One row of `sqlite_schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    /// What kind of object this is.
    pub obj_type: ObjectType,
    /// The object's name.
    pub name: String,
    /// The table this object is attached to.
    pub tbl_name: String,
    /// The b-tree root page (0 for views and triggers).
    pub rootpage: u32,
    /// The `CREATE` statement text, if any.
    pub sql: Option<String>,
}

/// The whole schema catalog: every object in `sqlite_schema`.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    objects: Vec<SchemaObject>,
}

impl Schema {
    /// Read and parse the entire `sqlite_schema` table from the database.
    pub fn read(pager: &dyn PageSource) -> Result<Schema> {
        let encoding = pager.header().text_encoding;
        let mut objects = Vec::new();
        let mut cur = TableCursor::new(pager, SCHEMA_ROOT_PAGE);
        let mut ok = cur.first()?;
        while ok {
            let cols = decode_record(&cur.payload()?, encoding)?;
            objects.push(parse_schema_row(&cols)?);
            ok = cur.next()?;
        }
        Ok(Schema { objects })
    }

    /// All objects, in `sqlite_schema` (rowid) order.
    pub fn objects(&self) -> &[SchemaObject] {
        &self.objects
    }

    /// Find a table by name (case-sensitive for now; SQLite folds ASCII case —
    /// tracked for Phase 9).
    pub fn table(&self, name: &str) -> Option<&SchemaObject> {
        self.find(ObjectType::Table, name)
    }

    /// Find an index by name.
    pub fn index(&self, name: &str) -> Option<&SchemaObject> {
        self.find(ObjectType::Index, name)
    }

    /// All indexes attached to the named table.
    pub fn indexes_on<'a>(&'a self, table: &'a str) -> impl Iterator<Item = &'a SchemaObject> + 'a {
        self.objects
            .iter()
            .filter(move |o| o.obj_type == ObjectType::Index && o.tbl_name == table)
    }

    fn find(&self, ty: ObjectType, name: &str) -> Option<&SchemaObject> {
        self.objects
            .iter()
            .find(|o| o.obj_type == ty && o.name == name)
    }
}

fn parse_schema_row(cols: &[Value]) -> Result<SchemaObject> {
    if cols.len() < 5 {
        return Err(Error::Corrupt(format!(
            "sqlite_schema row has {} columns, expected 5",
            cols.len()
        )));
    }
    let text = |v: &Value| -> Result<String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            _ => Err(Error::Corrupt("expected text in sqlite_schema".into())),
        }
    };
    let obj_type = ObjectType::parse(&text(&cols[0])?)?;
    let name = text(&cols[1])?;
    let tbl_name = text(&cols[2])?;
    let rootpage = match &cols[3] {
        Value::Integer(i) => *i as u32,
        Value::Null => 0,
        _ => {
            return Err(Error::Corrupt(
                "sqlite_schema rootpage not an integer".into(),
            ))
        }
    };
    let sql = match &cols[4] {
        Value::Text(s) => Some(s.clone()),
        Value::Null => None,
        _ => return Err(Error::Corrupt("sqlite_schema sql not text".into())),
    };
    Ok(SchemaObject {
        obj_type,
        name,
        tbl_name,
        rootpage,
        sql,
    })
}
