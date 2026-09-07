//! What is left of `services/` after the rest moved to `komo-services`:
//! `operator_control` reaches up into `agent::daemon` (for a cron job's next
//! occurrence and for dreaming) and out through `infra::gateway_client`, so it
//! cannot sit below either.
pub mod operator_control;
