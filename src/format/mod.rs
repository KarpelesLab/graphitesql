//! The SQLite version 3 on-disk file format.
//!
//! This module is graphitesql's compatibility surface: the byte-exact layout of
//! the database file. It is built bottom-up so each layer can be tested against
//! real SQLite-produced files before the next is added. The authoritative
//! reference is <https://www.sqlite.org/fileformat2.html> (mirrored locally by
//! `reference/fetch.sh`).
//!
//! Layers (see `ROADMAP.md` for status):
//!
//! * [`header`] — the 100-byte database header. **(implemented)**
//! * page parsing — table/index, interior/leaf b-tree pages. *(planned)*
//! * cells & records — payload, overflow, the record serial format. *(planned)*
//! * freelist & pointer maps. *(planned)*

pub mod header;
pub mod record;

pub use header::{DatabaseHeader, TextEncoding};
pub use record::{decode_record, encode_record};
