//! Host networking: port allocation, deterministic subnets, microVM networks
//! (TAP + socat publish with guest FORWARD isolation, or unprivileged passt).

mod forward;
mod microvm_net;
mod ports;
mod probe;
mod subnet;
mod tap;

// Re-export the pre-split public surface so callers keep `crate::network::…`.
pub use forward::{
    FORWARD_FILTER_ALLOW_RISK, RSL_IFACE_MATCH, RUSSEL_FORWARD_CHAIN, ensure_forward_filter,
    ensure_forward_filter_if_taps_present, forward_filter_disabled,
    forward_filter_disabled_from_env, restore_forward_filter, restore_ip_forward,
};
pub use microvm_net::{
    MICROVM_READY_TIMEOUT, MicrovmNet, MicrovmNetMode, NET_MODE_KEY, PASST_PID_KEY, PASST_SOCKET,
    VmNet,
};
pub use ports::{PortAllocator, publish_bind_addr};
pub use probe::app_accepts;
pub use subnet::{
    SubnetAllocation, allocation_from_network_key, claim_subnet_key, lookup_subnet,
    network_key_from_host_ip, preferred_subnet, release_subnet, subnet_for,
};
pub use tap::TapForwarder;
pub(crate) use tap::run_ip;

#[cfg(test)]
pub use ports::{port_test_lock, reserve_test_port};
#[cfg(test)]
pub use subnet::{subnet_test_lock, test_clear_subnet_registry, test_with_empty_registry};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
