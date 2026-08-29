//! Git clone, checkout lease, and host allowlist helpers.

mod allowlist;
mod client;
mod gc;
mod lease;
mod redact;

// Preserve `crate::git::{GitClient, CheckoutLease, gc_old_checkouts}` paths.
// Some symbols are only named by external callers / type inference; keep re-exports.
#[allow(unused_imports)]
pub use client::GitClient;
#[allow(unused_imports)]
pub use gc::gc_old_checkouts;
#[allow(unused_imports)]
pub use lease::CheckoutLease;
pub use redact::redact_repo_url;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
