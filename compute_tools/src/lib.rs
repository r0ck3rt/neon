//! Various tools and helpers to handle cluster / compute node (Postgres)
//! configuration.
#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod checker;
pub mod communicator_socket_client;
pub mod config;
pub mod configurator;
pub mod http;
#[macro_use]
pub mod logger;
pub mod catalog;
pub mod compute;
pub mod compute_prewarm;
pub mod compute_promote;
pub mod disk_quota;
pub mod extension_server;
pub mod hadron_metrics;
pub mod installed_extensions;
pub mod local_proxy;
pub mod lsn_lease;
pub mod metrics;
mod migration;
pub mod monitor;
pub mod params;
pub mod pg_helpers;
pub mod pg_isready;
pub mod pgbouncer;
pub mod rsyslog;
pub mod spec;
mod spec_apply;
pub mod swap;
pub mod sync_sk;
pub mod tls;
