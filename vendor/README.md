# vendor/

Crates komo patches via `[patch.crates-io]` in the workspace `Cargo.toml`.
Each directory is an unmodified copy of the published crate except for the
change described here; keep the diff to one hunk so an upstream bump is a
re-copy plus re-applying it.

| crate | version | change | why |
|---|---|---|---|
| `wechatbot` | 0.4.0 (MIT) | `Cargo.toml`: reqwest `0.12` default features → `0.13`, `default-features = false`, `["json", "rustls-no-provider", "http2", "charset"]` | upstream inherits reqwest's default `native-tls`, which needs `openssl-devel` on every build host and puts an OpenSSL C build on the cold-build critical path; `-no-provider` reuses the ring provider the `komo` bin installs (`docs/komo_bot.md` §13.4, `.scratch/komo-v08-rewrite/spikes/wechat.md`) |
| `toasty-driver-turso` | 0.10.0 (MIT) | `Cargo.toml`: its `turso` dependency gets `default-features = false, features = ["mimalloc"]` | turso's default `fts` feature pulls tantivy / zstd-sys / lz4 and a C toolchain for an index method MVCC mode refuses to create anyway; 62 fewer compile units (`spikes/store.md`). CI asserts `cargo tree -e features -i turso` shows only `mimalloc` |
