//! The I/O side of komo that depends on nothing above it: storage, the
//! filesystem-backed stores, and the two outbound integrations that are pure
//! adapters.
//!
//! Everything here reads only `komo-core`'s traits and value types plus
//! `komo-config`, which is what lets it compile before the agent, the tools, and
//! the services that use it. The parts of the old `infra/` that reach *upward* —
//! the chat channels (they dispatch turns), `llm.rs` (it assembles the system
//! prompt), `gateway_client`/`skill_install` (operator actions) — deliberately
//! stayed in the binary; they are wiring, not infrastructure.

pub mod chunk_index;
pub mod codex;
pub mod embedding;
pub mod logs;
pub mod memory;
pub mod permissions_store;
pub mod persistence;
pub mod skill_install;
pub mod skills;
pub mod workday;
