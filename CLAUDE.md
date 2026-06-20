# Project working agreement (graphitesql)

A pure-Rust, `#![forbid(unsafe_code)]`, `#![no_std]`+alloc, zero-dependency
reimplementation of SQLite, byte-compatible with the SQLite 3 file format.
MSRV 1.88.

## CHANGELOG.md is owned by release-plz — NEVER edit it by hand

`CHANGELOG.md` is generated automatically by **release-plz** from
conventional-commit messages. The `## [Unreleased]` heading is an **empty
placeholder**; release-plz fills it in (as a new versioned section) on each
release. Editing `CHANGELOG.md` manually duplicates and conflicts with what
release-plz generates — don't do it.

- To add a changelog entry, **write a good conventional-commit message** instead.
- `ROADMAP.md` is *not* release-plz-managed — edit it freely to track progress.

## Conventional commits (required)

Every commit subject must follow `type(optional-scope): description`. The mapping
to changelog sections is configured in `release-plz.toml`:

| type | changelog section |
|------|-------------------|
| `feat:` | Added |
| `fix:` | Fixed |
| `perf:` | Performance |
| `refactor:` | Refactor |
| `docs:` | Documentation |
| `test:` | Testing |
| `ci:` `build:` `chore:` `style:` `revert:`, and merge commits | **skipped** (no changelog entry) |
| anything else | Other |

Examples: `feat(json): add json_pretty(X [, indent])`,
`fix(parser): NATURAL JOIN was parsed as a table alias`,
`perf(planner): seek the inner join table by rowid`, `docs: update ROADMAP`,
`chore: tooling config` (skipped).

End commit messages (and PR bodies) with:
```
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
```

## Parallel worktree agents

When fanning out work across isolated worktree agents, give each a file-disjoint
slice and tell it **not** to touch `CHANGELOG.md` or `ROADMAP.md`. When
integrating an agent's branch, **squash-merge it into a single conventional
commit** (`git merge --squash <branch>` then commit with a `feat:`/`fix:`/…
message) rather than a `Merge …:` commit, so the changelog reads cleanly.

## The gate (run before every push; this is what CI runs)

CI installs a **pinned sqlite3 3.50.4** as the differential oracle (the distro
apt build is too old — see `.github/workflows/ci.yml`). Run the same locally:

1. `cargo fmt`
2. `cargo clippy --all-targets --all-features` — 0 warnings
3. `cargo test --all-features` — all pass (incl. the differential corpus vs
   `sqlite3`, which must stay green). **Use `--all-features`**, not plain
   `cargo test`, so local == CI.
4. `cargo build --no-default-features` — the `no_std` build must compile
5. `cargo doc --no-deps` — 0 warnings

## Hard constraints

No `unsafe`, no external dependencies in the default build (the lone exception is
the in-house `timezone-data` crate behind an opt-in feature), `no_std`+`alloc`
only. Every feature lands with a differential test against the real `sqlite3`
CLI, and anything written must pass `PRAGMA integrity_check`.
