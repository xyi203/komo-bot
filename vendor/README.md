# vendor/

Crates komo patches via `[patch.crates-io]` in the workspace `Cargo.toml`.
A vendored directory is an unmodified copy of the published crate except for one
hunk, so an upstream bump is a re-copy plus re-applying it.

Nothing is vendored right now. `wechatbot` was patched here until the chat channels were deleted (2026-11-13, `docs/redesign.md` P17).

`toasty-driver-turso` used to be vendored here only to turn off turso's default `fts` feature; since 0.11.0 upstream declares `turso` with `default-features = false`, so the copy was dropped. The workspace `turso` must stay on the same major as the driver's pin (0.11 → `^0.7`) or two copies get linked and their C symbols collide.
