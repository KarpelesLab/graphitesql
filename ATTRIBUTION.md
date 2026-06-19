# Attribution & Provenance

## SQLite — the reference

graphitesql re-implements the behavior and on-disk format of **SQLite**, the
public-domain database engine by D. Richard Hipp and contributors.

SQLite's authors disclaimed copyright and placed the work in the public domain,
leaving this blessing in place of a legal notice:

> May you do good and not evil.
> May you find forgiveness for yourself and forgive others.
> May you share freely, never taking more than you give.

We are deeply grateful for SQLite's existence, its meticulous documentation, and
its decades of stewardship. graphitesql exists to bring that design to
environments where a pure, safe, `no_std`-capable Rust implementation is
valuable (e.g. WebAssembly, embedded, sandboxed hosts).

## What we use, and how

| Upstream artifact | How graphitesql uses it | Linked into the crate? |
|---|---|---|
| SQLite C source (`btree.c`, `pager.c`, `vdbe*.c`, `parse.y`, …) | Read as a specification of correct behavior | **No** |
| SQLite file-format docs (`fileformat2.html`) | The authoritative spec for byte-level compatibility | No |
| SQLite SQL syntax docs | Specification of the dialect we accept | No |
| SQLite's TCL/SQL test corpus (future) | Differential / compatibility testing | No (dev-only) |

No SQLite source code is copied into, vendored by, or compiled into graphitesql.
The reference material is downloaded on demand by `reference/fetch.sh` and is
git-ignored.

## Pinned upstream version

- **SQLite version:** 3.53.2
- **Source tree:** `sqlite-src-3530200.zip`
- **SHA3-256:** `490ec7af32a6bfa5f3e05dc279c04286cfe3f328def4a8b7344e3fa20be18a4c`
- **File format:** SQLite database file format version 3 (compatible across all
  3.x releases; the format has been stable and back/forward compatible since 2004).

## graphitesql's own license

graphitesql is **public domain**, mirroring SQLite. In place of a legal notice
it carries the SQLite blessing (see `LICENSE`). The SPDX identifier is
`blessing`. Contributions are accepted on the same public-domain terms.
