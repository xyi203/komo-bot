//! komo-core: the dependency-light heart of komo.
//!
//! Holds the pure domain layer only — value types plus the repository and port
//! trait signatures, no I/O and no runtime. Everything above it (config
//! resolution, storage, the agent, the channels) depends on this crate; it
//! depends on nothing of komo's.

pub mod domain;
