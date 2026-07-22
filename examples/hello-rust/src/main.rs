//! Minimal HTTP server using only the Rust standard library.
//! Listens on PORT (default 3000). Endpoints: GET /health, GET /.
//! Exits cleanly on SIGTERM/SIGINT so `podman stop` does not hang.

use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

fn main() {
    install_term_handlers();

    let port = env::var("PORT").unwrap_or_else(|_| "3000".into());
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("bind {addr}: {e}");
        std::process::exit(1);
    });
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("nonblocking listen: {e}");
        std::process::exit(1);
    }
    eprintln!("hello-rust listening on {addr}");

    while RUNNING.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                thread::spawn(move || {
                    if let Err(e) = handle(stream) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("accept error: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    eprintln!("hello-rust shutting down");
}

#[cfg(unix)]
fn install_term_handlers() {
    // libc without the crates.io `libc` package — link system libc.
    #[allow(non_camel_case_types)]
    type sighandler_t = usize;
    unsafe extern "C" {
        fn signal(sig: i32, handler: sighandler_t) -> sighandler_t;
    }
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    // SAFETY: handler only flips an AtomicBool.
    unsafe {
        signal(SIGTERM, handle_signal as sighandler_t);
        signal(SIGINT, handle_signal as sighandler_t);
    }
}

#[cfg(unix)]
extern "C" fn handle_signal(_: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

#[cfg(not(unix))]
fn install_term_handlers() {}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status, body, ctype) = match path {
        "/health" => ("200 OK", "ok\n", "text/plain"),
        "/" => (
            "200 OK",
            "hello from russel hello-rust\n",
            "text/plain; charset=utf-8",
        ),
        _ => ("404 Not Found", "not found\n", "text/plain"),
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    Ok(())
}
