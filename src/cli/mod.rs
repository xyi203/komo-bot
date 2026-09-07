mod app;

/// The version string every surface reports — see [`app::VERSION`].
pub use app::VERSION;
mod channel;
mod doctor;
mod dream;
mod gateway;
mod health;
mod init;
mod inspect;
mod logs;
mod memory;
mod model;
mod pair;
mod policy;
mod service;
mod skill;
mod upgrade;
mod wechat;
mod wiki;
pub(crate) mod wiring;

pub use app::run;
