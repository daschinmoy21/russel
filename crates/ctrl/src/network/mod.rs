//! Host networking: port allocation, deterministic subnets, TAP + socat publish,
//! and guest FORWARD isolation.

mod forward;
mod ports;
mod subnet;
mod tap;

// Re-export the pre-split public surface so callers keep `crate::network::…`.
// Some items are only used inside this module tree today; keep them public for API stability.
#[allow(unused_imports)]
pub use forward::{
    FORWARD_FILTER_ALLOW_RISK, RSL_IFACE_MATCH, RUSSEL_FORWARD_CHAIN, ensure_forward_filter,
    ensure_forward_filter_if_taps_present, forward_filter_disabled,
    forward_filter_disabled_from_env, restore_forward_filter, restore_ip_forward,
};
pub use ports::{PortAllocator, publish_bind_addr};
pub use subnet::{
    SubnetAllocation, allocation_from_network_key, claim_subnet_key, network_key_from_host_ip,
    release_subnet, subnet_for,
};
pub use tap::TapForwarder;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
