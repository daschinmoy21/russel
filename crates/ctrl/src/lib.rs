//! `russel-ctrl` library crate.
//!
//! All control-plane logic lives here so the `russel-ctrl` binary and the
//! `russel-agent` worker share it: the agent's lifecycle RPC handlers call
//! the same in-process runners / metadata readers the monolithic control
//! plane uses (#214).
//!
//! clippy::type_complexity / too_many_arguments: deploy/lifecycle signatures
//! are wide by design (runtime + process handoff); silence until those APIs
//! are split.

#![allow(clippy::type_complexity, clippy::too_many_arguments)]

pub mod agent_client;
pub mod api;
pub mod build;
pub mod ch_api;
pub mod container;
pub mod deploy;
pub mod deployments;
pub mod git;
pub mod health;
pub mod ingress;
pub mod metadata;
pub mod microvm;
pub mod network;
pub mod reconcile;
pub mod runtime;
pub mod secrets;
pub mod state;
pub mod traefik;
pub mod warm_pool;

#[cfg(not(target_os = "linux"))]
compile_error!(
    "russel-ctrl requires Linux — it depends on cloud-hypervisor, iptables, socat, and TAP networking"
);
