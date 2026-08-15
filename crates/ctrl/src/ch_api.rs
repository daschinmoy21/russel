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

/// Maximum accumulated response-header size before the reader bails (64 KiB).
const MAX_RESPONSE_HEADER: usize = 64 * 1024;

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

/// Build the raw HTTP PUT request bytes (headers only) for a CH endpoint.
///
/// The body (if any) is written separately by [`put_request`]; its byte length
/// is reflected in the `Content-Length` header.
fn build_put_request(endpoint: &str, body_len: usize) -> String {
    format!(
        "PUT /api/v1/{endpoint} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {body_len}\r\n\
         Connection: close\r\n\
         \r\n"
    )
}

/// Append one chunk to the response-header buffer.
///
/// Returns `Ok(true)` once the `\r\n\r\n` header terminator has been observed
/// (any response body after the terminator is intentionally ignored — the CH
/// PUT endpoints return empty/no-content bodies). Returns `Ok(false)` while
/// more header bytes are expected. Rejects a buffer that grows past
/// [`MAX_RESPONSE_HEADER`] without a terminator.
fn push_response_chunk(endpoint: &str, buffer: &mut Vec<u8>, chunk: &[u8]) -> anyhow::Result<bool> {
    buffer.extend_from_slice(chunk);
    if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        return Ok(true);
    }
    if buffer.len() > MAX_RESPONSE_HEADER {
        anyhow::bail!("response headers from {endpoint} are too large");
    }
    Ok(false)
}

/// Extract and validate the HTTP status code from a raw response buffer.
fn parse_http_status(response: &[u8], endpoint: &str) -> anyhow::Result<u16> {
    let status_line = String::from_utf8_lossy(response)
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

    Ok(status)
}

async fn put_request(
    api_socket: &Path,
    endpoint: &str,
    body: Option<&serde_json::Value>,
) -> anyhow::Result<()> {
    let body_bytes = match body {
        Some(v) => serde_json::to_vec(v)?,
        None => Vec::new(),
    };

    let request = build_put_request(endpoint, body_bytes.len());

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
            if push_response_chunk(endpoint, &mut response, &chunk[..read])? {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for {endpoint}"))??;

    parse_http_status(&response, endpoint)?;

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const FULL_OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";

    #[test]
    fn build_put_request_exact_bytes() {
        // Empty-body PUT (e.g. vm.pause): no body bytes, Content-Length 0.
        assert_eq!(
            build_put_request("vm.pause", 0),
            "PUT /api/v1/vm.pause HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\
             \r\n"
        );
        // JSON-body PUT (e.g. vm.snapshot): body length reflected exactly.
        let body = serde_json::json!({"destination_url": "file:///tmp/snap"});
        let len = serde_json::to_vec(&body).unwrap().len();
        let req = build_put_request("vm.snapshot", len);
        assert!(req.starts_with("PUT /api/v1/vm.snapshot HTTP/1.1\r\n"));
        assert!(req.contains(&format!("Content-Length: {len}\r\n")));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn parse_full_response_in_one_chunk() {
        let mut buf = Vec::new();
        assert!(push_response_chunk("vm.pause", &mut buf, FULL_OK.as_bytes()).unwrap());
        assert_eq!(parse_http_status(&buf, "vm.pause").unwrap(), 200);
    }

    #[test]
    fn parse_response_split_mid_line_and_mid_crlf() {
        // Split across every possible byte boundary; the scanner must only
        // report completion once the full \r\n\r\n terminator is present.
        let full = FULL_OK.as_bytes();
        for split_at in 0..full.len() {
            let mut buf = Vec::new();
            let (head, tail) = full.split_at(split_at);
            assert!(
                !push_response_chunk("vm.pause", &mut buf, head).unwrap(),
                "split at {split_at} must not complete on partial headers"
            );
            assert!(push_response_chunk("vm.pause", &mut buf, tail).unwrap());
            assert_eq!(parse_http_status(&buf, "vm.pause").unwrap(), 200);
        }
    }

    #[test]
    fn parse_ignores_body_after_terminator() {
        // A response body following the header terminator is ignored; only the
        // status line is used. (CH PUT endpoints return no content, but a
        // hostile/nonconforming peer may append a body.)
        let mut buf = Vec::new();
        let response = "HTTP/1.1 204 No Content\r\nContent-Length: 9\r\n\r\nsome body";
        assert!(push_response_chunk("vmm.shutdown", &mut buf, response.as_bytes()).unwrap());
        assert_eq!(parse_http_status(&buf, "vmm.shutdown").unwrap(), 204);
    }

    #[test]
    fn parse_non_2xx_status_bails() {
        let mut buf = Vec::new();
        let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
        assert!(push_response_chunk("vm.snapshot", &mut buf, response.as_bytes()).unwrap());
        let err = parse_http_status(&buf, "vm.snapshot").unwrap_err();
        assert!(err.to_string().contains("HTTP 400"));
    }

    #[test]
    fn parse_malformed_status_line_bails() {
        let mut buf = Vec::new();
        let response = "NOT-HTTP GARBAGE\r\n\r\n";
        assert!(push_response_chunk("vm.pause", &mut buf, response.as_bytes()).unwrap());
        let err = parse_http_status(&buf, "vm.pause").unwrap_err();
        assert!(err.to_string().contains("invalid response"));
    }

    #[test]
    fn parse_empty_read_bails() {
        // A stream that closes immediately (no bytes) yields an empty buffer,
        // which cannot produce a status code.
        let buf = Vec::new();
        assert!(parse_http_status(&buf, "vm.pause").is_err());
    }

    #[test]
    fn oversized_headers_rejected() {
        // > 64 KiB without a terminator must be rejected rather than growing
        // without bound.
        let mut buf = Vec::new();
        let chunk = vec![b'x'; 1024];
        let mut result = None;
        for _ in 0..80 {
            match push_response_chunk("vm.pause", &mut buf, &chunk) {
                Ok(true) => {
                    result = Some(Ok(()));
                    break;
                }
                Ok(false) => {}
                Err(e) => {
                    result = Some(Err(e));
                    break;
                }
            }
        }
        let err = result
            .expect("loop must terminate in success or error")
            .unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn terminator_before_size_limit_wins() {
        // A chunk that crosses the size cap but also contains the terminator is
        // accepted (terminator check runs first, matching the reader loop).
        let mut buf = Vec::new();
        let mut response = vec![b'y'; MAX_RESPONSE_HEADER + 10];
        response.extend_from_slice(b"\r\n\r\n");
        assert!(push_response_chunk("vm.pause", &mut buf, &response).unwrap());
    }
}
