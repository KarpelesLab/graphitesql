//! Error and result types.
//!
//! graphitesql mirrors SQLite's primary result codes so that callers familiar
//! with SQLite get predictable, recognizable errors. The extended result codes
//! will be layered on as the engine grows (tracked in `ROADMAP.md`).

use alloc::string::String;
use core::fmt;

/// A `Result` whose error is graphitesql's [`Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// An error returned by graphitesql.
///
/// Variants are named after the corresponding SQLite primary result codes
/// (`SQLITE_*`) to keep the mapping obvious. [`Error::code`] returns the numeric
/// code SQLite would use, which is handy for compatibility shims and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Generic error (`SQLITE_ERROR`), with a human-readable message.
    Error(String),
    /// The database file is malformed (`SQLITE_CORRUPT`).
    Corrupt(String),
    /// A disk I/O error occurred in the VFS (`SQLITE_IOERR`).
    Io(String),
    /// The database file is locked (`SQLITE_BUSY`).
    Busy,
    /// Access permission denied (`SQLITE_PERM` / `SQLITE_CANTOPEN`).
    CantOpen(String),
    /// A constraint violation (`SQLITE_CONSTRAINT`).
    Constraint(String),
    /// SQL could not be tokenized or parsed, or a logic error in SQL
    /// (`SQLITE_ERROR`, surfaced separately for clearer diagnostics).
    Parse(String),
    /// An operation was attempted that this build does not yet implement.
    ///
    /// Not a SQLite code; it exists so the engine can fail loudly and
    /// specifically while under construction rather than silently misbehave.
    Unsupported(&'static str),
}

impl Error {
    /// The SQLite primary result code corresponding to this error.
    ///
    /// [`Error::Unsupported`] maps to `SQLITE_ERROR` (1) since SQLite has no
    /// equivalent concept.
    pub fn code(&self) -> i32 {
        match self {
            Error::Error(_) | Error::Parse(_) | Error::Unsupported(_) => 1, // SQLITE_ERROR
            Error::Corrupt(_) => 11,                                        // SQLITE_CORRUPT
            Error::Io(_) => 10,                                             // SQLITE_IOERR
            Error::Busy => 5,                                               // SQLITE_BUSY
            Error::CantOpen(_) => 14,                                       // SQLITE_CANTOPEN
            Error::Constraint(_) => 19,                                     // SQLITE_CONSTRAINT
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Error(m) => write!(f, "error: {m}"),
            Error::Corrupt(m) => write!(f, "database disk image is malformed: {m}"),
            Error::Io(m) => write!(f, "disk I/O error: {m}"),
            Error::Busy => write!(f, "database is locked"),
            Error::CantOpen(m) => write!(f, "unable to open database file: {m}"),
            // The message already names the specific constraint (`UNIQUE
            // constraint failed: t.a`, `CHECK constraint failed: …`, a `RAISE()`
            // string, the STRICT `cannot store …` text), matching sqlite's
            // `errmsg` verbatim — so no redundant outer prefix is added.
            Error::Constraint(m) => write!(f, "{m}"),
            Error::Parse(m) => write!(f, "SQL error: {m}"),
            Error::Unsupported(m) => write!(f, "not yet implemented: {m}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}
