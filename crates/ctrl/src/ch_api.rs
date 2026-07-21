//! Cloud Hypervisor REST API client over Unix-domain socket.
//!
//! Every helper takes an `api_socket: &Path` so callers are not tied to
//! a single hardcoded socket layout.  The `ChClient` struct is a thin
//! convenience wrapper for when a persistent socket path is preferred.
//!
//! Supported endpoints:
//!   - empty-body PUT: `vm.pause`, `vm.resume`, `vm.shutdown`, `vmm.shutdown`
//!   - JSON-body PUT:  `vm.snapshot` (and future `vm.resize`, device add)
//!
//! This replaces the ad-hoc `cloud_hypervisor_api_request` in `microvm.rs`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Convenience handle for a single Cloud Hypervisor API socket.
#[derive(Debug, Clone)]
pub struct ChClient {
    api_socket: PathBuf,
}

impl ChClient {
    pub fn new(api_socket: &Path) -> Self {
        Self {
            api_socket: api_socket.to_path_buf(),
        }
    }

    /// Accessor for callers that need the raw socket path (e.g. logging, handoff).
    #[allow(dead_code)] // public API surface; not all call sites use it yet
    pub fn socket(&self) -> &Path {
        &self.api_socket
    }
}

// ── Empty-body PUT ───────────────────────────────────────────────────────────

/// Send an empty-body PUT to a Cloud Hypervisor API endpoint.
///
/// Used for: `vm.pause`, `vm.resume`, `vm.shutdown`, `vmm.shutdown`, etc.
pub async fn empty_put(api_socket: &Path, endpoint: &str) -> anyhow::Result<()> {
    ChClient::new(api_socket).empty_put(endpoint).await
}

impl ChClient {
    pub async fn empty_put(&self, endpoint: &str) -> anyhow::Result<()> {
        put_request(&self.api_socket, endpoint, None).await
    }
}

// ── JSON-body PUT ────────────────────────────────────────────────────────────

/// Send a JSON-body PUT to a Cloud Hypervisor API endpoint.
///
/// Used for: `vm.snapshot` (`{"destination_url":"file:///…"}`),
/// and future `vm.resize`, `vm.add-device`, etc.
pub async fn json_put(
    api_socket: &Path,
    endpoint: &str,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    ChClient::new(api_socket).json_put(endpoint, body).await
}

impl ChClient {
    pub async fn json_put(&self, endpoint: &str, body: &serde_json::Value) -> anyhow::Result<()> {
        put_request(&self.api_socket, endpoint, Some(body)).await
    }
}

// ── VM lifecycle helpers ─────────────────────────────────────────────────────

/// Pause a running VM (prep for snapshot).
pub async fn vm_pause(api_socket: &Path) -> anyhow::Result<()> {
    empty_put(api_socket, "vm.pause").await
}

/// Resume a paused VM (pair of `vm_pause`; used after snapshot restore).
#[allow(dead_code)] // wired when warm-pool restore path lands
pub async fn vm_resume(api_socket: &Path) -> anyhow::Result<()> {
    empty_put(api_socket, "vm.resume").await
}

/// Ask the guest to shut down gracefully.
pub async fn vm_shutdown(api_socket: &Path) -> anyhow::Result<()> {
    empty_put(api_socket, "vm.shutdown").await
}

/// Ask the VMM process to exit.
pub async fn vmm_shutdown(api_socket: &Path) -> anyhow::Result<()> {
    empty_put(api_socket, "vmm.shutdown").await
}

/// Take a VM snapshot.
pub async fn vm_snapshot(api_socket: &Path, destination_url: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({"destination_url": destination_url});
    json_put(api_socket, "vm.snapshot", &body).await
}

// ── Internal HTTP PUT ────────────────────────────────────────────────────────

async fn put_request(
    api_socket: &Path,
    endpoint: &str,
    body: Option<&serde_json::Value>,
) -> anyhow::Result<()> {
    let body_bytes = match body {
        Some(v) => serde_json::to_vec(v)?,
        None => Vec::new(),
    };

    let request = format!(
        "PUT /api/v1/{endpoint} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body_bytes.len(),
    );

    let mut stream = tokio::time::timeout(DEFAULT_TIMEOUT, UnixStream::connect(api_socket))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out connecting to CH API socket {}",
                api_socket.display()
            )
        })??;

    tokio::time::timeout(DEFAULT_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| anyhow::anyhow!("timed out writing {endpoint}"))??;

    if !body_bytes.is_empty() {
        tokio::time::timeout(DEFAULT_TIMEOUT, stream.write_all(&body_bytes))
            .await
            .map_err(|_| anyhow::anyhow!("timed out writing body for {endpoint}"))??;
    }

    let mut response = Vec::with_capacity(1024);
    tokio::time::timeout(DEFAULT_TIMEOUT, async {
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..read]);
            if response.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if response.len() > 64 * 1024 {
                anyhow::bail!("response headers from {endpoint} are too large");
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for {endpoint}"))??;

    let status_line = String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();

    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid response from {endpoint}: {status_line}"))?;

    if !(200..300).contains(&status) {
        anyhow::bail!("Cloud Hypervisor returned HTTP {status} for {endpoint}");
    }

    Ok(())
}
