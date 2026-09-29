//! "Did the app accept this connection?" through a user-space port forwarder.
//!
//! Rootless Podman publishes ports with pasta (or rootlessport), and microVMs
//! with socat. The forwarder owns the host port and accepts every connection,
//! then dials the app. When nothing listens behind it (the app crashed, has
//! not bound yet, or bound 127.0.0.1 inside the container), it closes the
//! accepted connection within milliseconds; measured with pasta on Podman 5.8,
//! about 15–20 ms. A listening app keeps the connection open waiting for the
//! client, or speaks first. So a bare connect proves nothing (#462), but a
//! connect that survives a short window does, for any protocol, without
//! sending the app a byte.

use std::time::Duration;

use tokio::io::AsyncReadExt;

/// How long an accepted connection must stay open to count as the app.
/// Forwarders close unbacked connections an order of magnitude faster.
pub const FORWARDER_CLOSE_WINDOW: Duration = Duration::from_millis(200);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether the peer at `addr` is a live app rather than a forwarder with
/// nothing behind it: the connect succeeds, and the connection is not closed
/// or reset within [`FORWARDER_CLOSE_WINDOW`] (or the peer sends data first).
pub async fn app_accepts(addr: &str) -> bool {
    app_accepts_within(addr, FORWARDER_CLOSE_WINDOW).await
}

pub(crate) async fn app_accepts_within(addr: &str, window: Duration) -> bool {
    let Ok(Ok(mut stream)) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await
    else {
        return false;
    };
    let mut byte = [0u8; 1];
    match tokio::time::timeout(window, stream.read(&mut byte)).await {
        // Still open: the app is waiting for the client to speak.
        Err(_elapsed) => true,
        // The app spoke first (a banner or greeting).
        Ok(Ok(n)) if n > 0 => true,
        // EOF or reset: the forwarder found nothing to dial.
        Ok(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    const WINDOW: Duration = Duration::from_millis(100);

    /// Accepts and immediately closes, like pasta with no listener behind it.
    async fn forwarder_with_nothing_behind() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        addr
    }

    /// Accepts and holds the connection, like an HTTP or Redis server.
    async fn silent_app() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        addr
    }

    #[tokio::test]
    async fn forwarder_that_closes_is_not_the_app() {
        let addr = forwarder_with_nothing_behind().await;
        assert!(!app_accepts_within(&addr, WINDOW).await);
    }

    #[tokio::test]
    async fn app_that_waits_for_the_client_is_ready() {
        let addr = silent_app().await;
        assert!(app_accepts_within(&addr, WINDOW).await);
    }

    #[tokio::test]
    async fn app_that_speaks_first_is_ready_without_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"220 hello\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let started = std::time::Instant::now();
        assert!(app_accepts_within(&addr, Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn refused_connect_is_not_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(!app_accepts_within(&addr, WINDOW).await);
    }
}
