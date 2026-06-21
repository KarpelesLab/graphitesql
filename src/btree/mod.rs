//! B-tree layer: the table and index trees that hold all user data.
//!
//! Built on the [`pager`](crate::pager), this module parses b-tree pages
//! ([`page`]) and provides cursors ([`cursor`]) that iterate and seek within a
//! tree. It deals in raw record *payloads* (byte slices); turning a payload into
//! typed column [`Value`](crate::Value)s is the `format::record` layer's job
//! (Phase 3).
//!
//! This phase is read-only. Insertion, deletion, and page balancing arrive in
//! Phase 6.

pub mod cursor;
pub mod index_writer;
pub mod page;
pub mod ptrmap;
pub mod writer;

pub use cursor::{IndexCursor, TableCursor};
pub use index_writer::{
    clear_index, create_index_root, free_tree, index_range_records, index_range_rowids,
    index_seek_records, index_seek_rowids, insert_index,
};
pub use page::{BtreePage, IndexCell, PageType, Payload, TableLeafCell};
pub use writer::{
    clear_table, create_table_root, delete_table, insert_table, table_has_empty_leaf,
};
