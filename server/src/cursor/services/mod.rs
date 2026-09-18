//! Exposes Cursor services outside the Agent loop.

pub mod account;
pub mod analytics;
pub mod blob_sync;
pub mod commit_message;
pub mod compatibility;
pub mod context_sync;
pub(crate) mod entitlement;
pub(crate) mod get_me_cache;
pub mod knowledge;
pub mod model_catalog;
pub mod observability;
pub mod plugin_catalog;
pub mod server_config;
pub mod startup_timing;
pub mod tab;
pub mod usage;
