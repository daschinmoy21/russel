//! Traffic through a dual-live update (#562). Clients hold keep-alive
//! connections to a Traefik stand-in and send short and long requests while
//! the route moves from one generation to the next, through the steps
//! `deploy_inner` takes: swap, wait until the proxy serves the new backend,
//! drain, retire. Every failed request and every broken client connection
//! counts as a drop.
//!
//! The stand-in reads the route files `TraefikFileIngress` writes the way
//! Traefik's file provider does, and applies a change [`RELOAD_DELAY`] after
//! it lands, as Traefik's `providersThrottleDuration` does. A generation is
//! an HTTP server plus a `sleep` process standing in for its VM: like the
//! guest init, it watches `cfg/stop` and exits once its app is done.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::{AbortHandle, JoinHandle},
};

use super::rollback::{STOP_FILE, retire_microvm};
use crate::ingress::{Backend, Ingress, Served};
use crate::traefik::{ROUTE_HEADER, TraefikFileIngress};

const SERVICE: &str = "api";
const HOST: &str = "api.russel.local";
const CLIENTS: usize = 6;
/// The stand-in applies a changed route file this long after it lands.
const RELOAD_DELAY: Duration = Duration::from_millis(400);
/// The pipeline's `DRAIN_WINDOW` and `RETIRE_GRACE`, scaled down.
const DRAIN: Duration = Duration::from_millis(2000);
const GRACE: Duration = Duration::from_millis(1500);
/// Every third request is this long: shorter than [`DRAIN`] and [`GRACE`],
/// longer than [`RELOAD_DELAY`].
const LONG_REQUEST_MS: u64 = 1200;
const SHORT_REQUEST_MS: u64 = 50;

/// One HTTP/1.1 message: the head lines and a Content-Length body.
struct Message {
    head: Vec<String>,
    body: Vec<u8>,
}

impl Message {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.iter().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn first_line(&self) -> &str {
        self.head.first().map_or("", String::as_str)
    }
}

async fn read_message<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> std::io::Result<Message> {
    let mut head = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        head.push(line.to_string());
    }
    let mut message = Message {
        head,
        body: Vec::new(),
    };
    let len = message
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    message.body = vec![0; len];
    reader.read_exact(&mut message.body).await?;
    Ok(message)
}

fn response(status: &str, extra_headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\n{extra_headers}Content-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// The route the stand-in serves: what Traefik takes from the file.
#[derive(Clone, Debug)]
struct Route {
    host: String,
    /// `host:port` of the load balancer's one server.
    backend: String,
    /// The route header the router's middleware adds.
    token: String,
}

fn parse_route(raw: &str) -> Option<Route> {
    let config: serde_json::Value = serde_json::from_str(raw).ok()?;
    let name = format!("russel-{SERVICE}");
    let http = &config["http"];
    let router = &http["routers"][&name];
    let host = router["rule"]
        .as_str()?
        .strip_prefix("Host(`")?
        .strip_suffix("`)")?
        .to_string();
    let backend = http["services"][&name]["loadBalancer"]["servers"][0]["url"]
        .as_str()?
        .strip_prefix("http://")?
        .to_string();
    let token = router["middlewares"]
        .as_array()?
        .iter()
        .find_map(|m| {
            http["middlewares"][m.as_str()?]["headers"]["customResponseHeaders"][ROUTE_HEADER]
                .as_str()
        })?
        .to_string();
    Some(Route {
        host,
        backend,
        token,
    })
}

/// A Traefik stand-in: file provider plus an HTTP entry point.
struct FakeTraefik {
    port: u16,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for FakeTraefik {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl FakeTraefik {
    async fn start(dynamic_dir: &Path) -> Self {
        let route: Arc<Mutex<Option<Route>>> = Arc::new(Mutex::new(None));

        let file = dynamic_dir.join(format!("{SERVICE}.yaml"));
        let loaded = route.clone();
        let provider = tokio::spawn(async move {
            let mut seen = String::new();
            loop {
                if let Ok(raw) = tokio::fs::read_to_string(&file).await
                    && raw != seen
                {
                    seen = raw;
                    tokio::time::sleep(RELOAD_DELAY).await;
                    *loaded.lock().unwrap() = parse_route(&seen);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let entrypoint = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let route = route.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut reader = BufReader::new(read);
                    while let Ok(request) = read_message(&mut reader).await {
                        let reply = proxy(&route, &request).await;
                        if write.write_all(&reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            port,
            tasks: vec![provider, entrypoint],
        }
    }
}

/// Route one request with the route loaded right now, over a fresh
/// connection to the backend. A backend that refuses or breaks off is a 502,
/// as in Traefik; the client's own connection stays open.
async fn proxy(route: &Mutex<Option<Route>>, request: &Message) -> Vec<u8> {
    let route = route.lock().unwrap().clone();
    let host = request
        .header("host")
        .map(|h| h.split(':').next().unwrap_or(h).to_ascii_lowercase());
    let Some(route) = route.filter(|r| host.as_deref() == Some(r.host.as_str())) else {
        return response("404 Not Found", "", b"404 page not found");
    };
    let marker = format!("{ROUTE_HEADER}: {}\r\n", route.token);
    let upstream = async {
        let mut backend = TcpStream::connect(&route.backend).await?;
        let head = format!(
            "{}\r\nHost: {}\r\nConnection: close\r\n\r\n",
            request.first_line(),
            route.host
        );
        backend.write_all(head.as_bytes()).await?;
        read_message(&mut BufReader::new(backend)).await
    }
    .await;
    let head_only = request.first_line().starts_with("HEAD ");
    match upstream {
        Ok(reply) => {
            let status = reply.first_line().split_once(' ').map_or("200 OK", |s| s.1);
            response(status, &marker, if head_only { b"" } else { &reply.body })
        }
        Err(_) => response("502 Bad Gateway", &marker, b"Bad Gateway"),
    }
}

/// What a generation's app does when its stop request arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnStop {
    /// Exits at once, like an app with no SIGTERM handler: requests in
    /// flight are cut.
    ExitAtOnce,
    /// Stops taking connections and finishes the requests it has.
    Finish,
    /// Ignores it.
    Ignore,
}

/// A generation: an HTTP server that answers `GET /sleep/<ms>` with its name
/// after `<ms>`, and a process standing in for its VM.
struct Generation {
    port: u16,
    dir: PathBuf,
    vm: Option<tokio::process::Child>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Generation {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Counts a request in flight until dropped, also when the task is aborted.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn start(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Generation {
    async fn boot(root: &Path, name: &str, on_stop: OnStop) -> Self {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("cfg")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let vm = tokio::process::Command::new("sleep")
            .arg("600")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let vm_pid = i32::try_from(vm.id().unwrap()).unwrap();

        let in_flight = Arc::new(AtomicUsize::new(0));
        let requests: Arc<Mutex<Vec<AbortHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let (count, handles, body) = (in_flight.clone(), requests.clone(), name.to_string());
        let app = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (count, body) = (count.clone(), body.clone());
                let request = tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let Ok(request) = read_message(&mut BufReader::new(read)).await else {
                        return;
                    };
                    let _in_flight = InFlight::start(&count);
                    let ms = request
                        .first_line()
                        .split_whitespace()
                        .nth(1)
                        .and_then(|path| path.strip_prefix("/sleep/"))
                        .and_then(|ms| ms.parse().ok())
                        .unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    let _ = write
                        .write_all(&response(
                            "200 OK",
                            "Connection: close\r\n",
                            body.as_bytes(),
                        ))
                        .await;
                });
                handles.lock().unwrap().push(request.abort_handle());
            }
        });

        // The guest init's part: turn the stop request into the app's exit,
        // then power off, which ends the VM process.
        let stop = dir.join("cfg").join(STOP_FILE);
        let app_handle = app.abort_handle();
        let init = tokio::spawn(async move {
            while !stop.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            match on_stop {
                OnStop::Ignore => return,
                OnStop::ExitAtOnce => {
                    app_handle.abort();
                    for request in requests.lock().unwrap().drain(..) {
                        request.abort();
                    }
                }
                OnStop::Finish => {
                    app_handle.abort();
                    while in_flight.load(Ordering::SeqCst) > 0 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
            // SAFETY: plain kill(2) on the child this generation spawned.
            unsafe { libc::kill(vm_pid, libc::SIGKILL) };
        });

        Self {
            port,
            dir,
            vm: Some(vm),
            tasks: vec![app, init],
        }
    }

    fn backend(&self) -> Backend {
        Backend::localhost(self.port)
    }
}

#[derive(Debug, Default)]
struct Counts {
    ok: usize,
    long_ok: usize,
    dropped: usize,
    connections: usize,
    by_generation: HashMap<String, usize>,
}

/// Send requests over one keep-alive connection until `stop`. A failed
/// request is a drop; a broken connection is a drop and a reconnect.
async fn client(proxy_port: u16, n: usize, stop: Arc<AtomicBool>) -> Counts {
    let mut counts = Counts::default();
    let mut conn = None;
    let mut i = n;
    while !stop.load(Ordering::SeqCst) {
        let ms = if i.is_multiple_of(3) {
            LONG_REQUEST_MS
        } else {
            SHORT_REQUEST_MS
        };
        i += 1;
        let (reader, writer) = match conn.as_mut() {
            Some(conn) => conn,
            None => {
                let (read, write) = TcpStream::connect(("127.0.0.1", proxy_port))
                    .await
                    .unwrap()
                    .into_split();
                counts.connections += 1;
                conn.insert((BufReader::new(read), write))
            }
        };
        let request = format!("GET /sleep/{ms} HTTP/1.1\r\nHost: {HOST}\r\n\r\n");
        let reply = match writer.write_all(request.as_bytes()).await {
            Ok(()) => read_message(reader).await,
            Err(e) => Err(e),
        };
        match reply {
            Ok(reply) if reply.first_line().contains(" 200 ") => {
                counts.ok += 1;
                if ms == LONG_REQUEST_MS {
                    counts.long_ok += 1;
                }
                *counts
                    .by_generation
                    .entry(String::from_utf8_lossy(&reply.body).into_owned())
                    .or_default() += 1;
            }
            failed => {
                counts.dropped += 1;
                if failed.is_err() {
                    conn = None;
                }
                // Back off a little so the count is requests, not a spin.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    counts
}

/// How much of the pipeline's cutover a run follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cutover {
    /// Wait for the proxy, drain, then retire: what `deploy_inner` does.
    Pipeline,
    /// Wait for the proxy, then retire at once.
    NoDrain,
    /// Retire as soon as the file is written (before #562).
    FileWritten,
}

#[derive(Debug, Default)]
struct Report {
    totals: Counts,
    retire_took: Duration,
}

/// Run clients through one update through the Traefik stand-in.
async fn update_under_traffic(cutover: Cutover, on_stop: OnStop) -> Report {
    let tmp = tempfile::tempdir().unwrap();
    let dynamic = tmp.path().join("dynamic");
    let traefik = FakeTraefik::start(&dynamic).await;
    update_through(&dynamic, traefik.port, tmp.path(), cutover, on_stop).await
}

/// Run clients through one update of `api` from generation `old` to `new`,
/// through the proxy on `proxy_port` that reads route files from `dynamic`.
/// The generations keep their files under `root`.
async fn update_through(
    dynamic: &Path,
    proxy_port: u16,
    root: &Path,
    cutover: Cutover,
    on_stop: OnStop,
) -> Report {
    let ingress = TraefikFileIngress::for_tests(
        dynamic,
        "russel.local",
        &format!("http://127.0.0.1:{proxy_port}"),
        Duration::from_secs(10),
    );

    let mut old = Generation::boot(root, "old", on_stop).await;
    ingress
        .register(SERVICE, &old.backend(), &[])
        .await
        .unwrap();
    assert_eq!(
        ingress
            .wait_served(SERVICE, &old.backend(), &[])
            .await
            .unwrap(),
        Served::Confirmed
    );

    let stop = Arc::new(AtomicBool::new(false));
    let clients: Vec<_> = (0..CLIENTS)
        .map(|n| tokio::spawn(client(proxy_port, n, stop.clone())))
        .collect();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The update, in `deploy_inner`'s order on the dual-live path.
    let new = Generation::boot(root, "new", on_stop).await;
    // swap_ingress
    ingress.swap(SERVICE, &new.backend(), &[]).await.unwrap();
    if cutover != Cutover::FileWritten {
        assert_eq!(
            ingress
                .wait_served(SERVICE, &new.backend(), &[])
                .await
                .unwrap(),
            Served::Confirmed
        );
    }
    let switched_at = Instant::now();
    // hold, then cutover's drain
    if cutover == Cutover::Pipeline {
        tokio::time::sleep(DRAIN.saturating_sub(switched_at.elapsed())).await;
    }
    // cutover's retire_processes
    let retiring = Instant::now();
    retire_microvm(&old.dir, old.vm.take(), Vec::new(), GRACE).await;
    let retire_took = retiring.elapsed();

    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, Ordering::SeqCst);
    let mut totals = Counts::default();
    for client in clients {
        let counts = client.await.unwrap();
        totals.ok += counts.ok;
        totals.long_ok += counts.long_ok;
        totals.dropped += counts.dropped;
        totals.connections += counts.connections;
        for (generation, n) in counts.by_generation {
            *totals.by_generation.entry(generation).or_default() += n;
        }
    }
    let report = Report {
        totals,
        retire_took,
    };
    eprintln!("{cutover:?} / {on_stop:?}: {report:?}");
    report
}

/// #562: the pipeline's cutover drops nothing, even for an app that exits
/// on SIGTERM without finishing its requests: the drain lets them end first.
#[tokio::test]
async fn dual_live_update_drops_no_requests_or_connections() {
    let report = update_under_traffic(Cutover::Pipeline, OnStop::ExitAtOnce).await;
    let totals = &report.totals;
    assert_eq!(totals.dropped, 0, "{report:?}");
    assert_eq!(totals.connections, CLIENTS, "a client connection broke");
    assert!(totals.by_generation["old"] > 0 && totals.by_generation["new"] > 0);
    assert!(totals.long_ok >= CLIENTS, "{report:?}");
    // Nothing was in flight on the old generation when it was asked to stop.
    assert!(report.retire_took < GRACE, "{report:?}");
}

/// With no drain, an app that finishes its requests on SIGTERM still loses
/// none: the grace period covers the requests in flight.
#[tokio::test]
async fn retire_grace_lets_a_graceful_app_finish() {
    let report = update_under_traffic(Cutover::NoDrain, OnStop::Finish).await;
    assert_eq!(report.totals.dropped, 0, "{report:?}");
    assert_eq!(report.totals.connections, CLIENTS);
}

/// The control: retiring once the file is written, as before #562, drops
/// requests, so the zero above is measured, not assumed.
#[tokio::test]
async fn retiring_before_the_proxy_switches_drops_requests() {
    let report = update_under_traffic(Cutover::FileWritten, OnStop::ExitAtOnce).await;
    assert!(report.totals.dropped > 0, "{report:?}");
}

/// A generation that ignores the stop request is killed when the grace
/// period runs out.
#[tokio::test]
async fn retire_kills_a_generation_that_ignores_the_stop_request() {
    let tmp = tempfile::tempdir().unwrap();
    let mut stuck = Generation::boot(tmp.path(), "stuck", OnStop::Ignore).await;
    let pid = stuck
        .vm
        .as_ref()
        .and_then(tokio::process::Child::id)
        .unwrap();
    let started = Instant::now();
    retire_microvm(&stuck.dir, stuck.vm.take(), Vec::new(), GRACE).await;
    assert!(started.elapsed() >= GRACE);
    assert!(stuck.dir.join("cfg").join(STOP_FILE).exists());
    assert!(crate::microvm::wait_for_process_exit(pid, Duration::ZERO).await);
}

/// A VM this ctrl did not start (it restarted since) is waited on through
/// the pid in its metadata.
#[tokio::test]
async fn retire_waits_on_the_recorded_pid_of_an_adopted_vm() {
    let tmp = tempfile::tempdir().unwrap();
    let adopted = Generation::boot(tmp.path(), "adopted", OnStop::Finish).await;
    let pid = adopted
        .vm
        .as_ref()
        .and_then(tokio::process::Child::id)
        .unwrap();
    std::fs::write(
        adopted.dir.join("metadata.json"),
        serde_json::json!({ "vm_pid": pid }).to_string(),
    )
    .unwrap();
    let started = Instant::now();
    retire_microvm(&adopted.dir, None, Vec::new(), GRACE).await;
    assert!(started.elapsed() < GRACE);
    assert!(crate::microvm::wait_for_process_exit(pid, Duration::ZERO).await);
}

/// The pipeline's update and the control, through a real Traefik when one
/// is given: `RUSSEL_TEST_TRAEFIK_DIR` is the directory its file provider
/// watches and `RUSSEL_TEST_TRAEFIK_PORT` its HTTP entry point on
/// 127.0.0.1. `contrib/tests/zero_downtime_traefik.sh` starts one and runs
/// this. One test, so the two updates of `api` do not overlap.
#[tokio::test]
#[ignore = "needs a running Traefik; see contrib/tests/zero_downtime_traefik.sh"]
async fn real_traefik_update_under_traffic() {
    let dir = PathBuf::from(std::env::var("RUSSEL_TEST_TRAEFIK_DIR").unwrap());
    let port: u16 = std::env::var("RUSSEL_TEST_TRAEFIK_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let report = update_through(
        &dir,
        port,
        tmp.path(),
        Cutover::Pipeline,
        OnStop::ExitAtOnce,
    )
    .await;
    let totals = &report.totals;
    assert_eq!(totals.dropped, 0, "{report:?}");
    assert_eq!(totals.connections, CLIENTS, "a client connection broke");
    assert!(totals.by_generation["old"] > 0 && totals.by_generation["new"] > 0);

    let control = tempfile::tempdir().unwrap();
    let report = update_through(
        &dir,
        port,
        control.path(),
        Cutover::FileWritten,
        OnStop::ExitAtOnce,
    )
    .await;
    assert!(report.totals.dropped > 0, "{report:?}");
}
