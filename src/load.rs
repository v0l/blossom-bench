use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use bytes::Bytes;
use hdrhistogram::Histogram;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::header::{CONNECTION, HeaderMap};
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

pub struct ReqSpec {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub ok_lo: u16,
    pub ok_hi: u16,
    pub expect_len: Option<u64>,
    pub follow_redirects: bool,
    pub resign: Option<std::sync::Arc<dyn Fn() -> Option<hyper::header::HeaderValue> + Send + Sync>>,
}

impl ReqSpec {
    fn build(&self) -> Request<Full<Bytes>> {
        let mut req = Request::new(Full::new(self.body.clone()));
        *req.method_mut() = self.method.clone();
        *req.uri_mut() = self.uri.clone();
        *req.headers_mut() = self.headers.clone();
        if let Some(token) = self.resign.as_ref().and_then(|f| f()) {
            req.headers_mut().insert(hyper::header::AUTHORIZATION, token);
        }
        req
    }
}

#[derive(Clone, Copy)]
pub enum Mode {
    Count,
    For(Duration),
}

pub struct FedReq {
    pub spec: ReqSpec,
    pub hash: String,
    pub size: u64,
}

pub struct Plan {
    pub byte_budget: Option<u64>,
    pub reqs: Vec<ReqSpec>,
    pub order: Option<Vec<u32>>,
    pub mode: Mode,
    pub connections: usize,
    pub feed: Option<async_channel::Receiver<FedReq>>,
    pub keep: usize,
}

pub struct Outcome {
    pub wall: Duration,
    pub total: u64,
    pub ok: u64,
    pub io_err: u64,
    pub bad_len: u64,
    pub bytes_in: u64,
    pub bytes_out_ok: u64,
    pub status: BTreeMap<u16, u64>,
    pub hist: Histogram<u64>,
    pub ok_idx: Vec<u32>,
    pub kept: Vec<(String, u64)>,
    pub error_sample: Option<String>,
}

struct Tally {
    total: u64,
    ok: u64,
    io_err: u64,
    bad_len: u64,
    bytes_in: u64,
    bytes_out_ok: u64,
    status: BTreeMap<u16, u64>,
    hist: Histogram<u64>,
    ok_idx: Vec<u32>,
    kept: Vec<(String, u64)>,
    error_sample: Option<String>,
}

impl Tally {
    fn new() -> Self {
        Self {
            total: 0,
            ok: 0,
            io_err: 0,
            bad_len: 0,
            bytes_in: 0,
            bytes_out_ok: 0,
            status: BTreeMap::new(),
            hist: Histogram::new_with_bounds(1, 120_000_000, 3).expect("histogram bounds"),
            ok_idx: Vec::new(),
            kept: Vec::new(),
            error_sample: None,
        }
    }
}

async fn connect(addr: SocketAddr) -> Result<http1::SendRequest<Full<Bytes>>> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let (sender, conn) = http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

struct Hop {
    status: u16,
    location: Option<String>,
    closing: bool,
    got: u64,
    head: Vec<u8>,
}

async fn read_response(resp: hyper::Response<hyper::body::Incoming>) -> Option<Hop> {
    let status = resp.status().as_u16();
    let closing = resp.headers().get(CONNECTION).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"close"));
    let location = resp.headers().get(hyper::header::LOCATION).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let mut body = resp.into_body();
    let mut got = 0u64;
    let mut head: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let f = frame.ok()?;
        if let Some(d) = f.data_ref() {
            got += d.len() as u64;
            if head.len() < 200 {
                let room = 200 - head.len();
                head.extend_from_slice(&d[..d.len().min(room)]);
            }
        }
    }
    Some(Hop { status, location, closing, got, head })
}

fn split_location(location: &str) -> Option<(String, String)> {
    let rest = location.strip_prefix("http://")?;
    Some(match rest.split_once('/') {
        Some((host, path)) => (host.to_string(), format!("/{path}")),
        None => (rest.to_string(), "/".to_string()),
    })
}

async fn follow(target: &mut Option<(String, http1::SendRequest<Full<Bytes>>)>, location: &str) -> Option<Hop> {
    let (host, path) = split_location(location)?;
    for _ in 0..2 {
        if target.as_ref().is_none_or(|(h, _)| *h != host) {
            let addr = tokio::net::lookup_host(host.as_str()).await.ok()?.next()?;
            *target = Some((host.clone(), connect(addr).await.ok()?));
        }
        let (_, sender) = target.as_mut()?;
        let req = Request::builder().method(Method::GET).uri(path.as_str()).header("host", host.as_str()).body(Full::new(Bytes::new())).ok()?;
        let sent = match sender.ready().await {
            Ok(()) => sender.send_request(req).await,
            Err(e) => Err(e),
        };
        match sent {
            Ok(resp) => {
                let hop = read_response(resp).await;
                if hop.as_ref().is_none_or(|h| h.closing) {
                    *target = None;
                }
                return hop;
            }
            Err(_) => *target = None,
        }
    }
    None
}

async fn worker(addr: SocketAddr, plan: Arc<Plan>, counter: Arc<AtomicUsize>, kept_count: Arc<AtomicUsize>, sent_bytes: Arc<std::sync::atomic::AtomicU64>, deadline: Option<Instant>) -> Tally {
    let mut tally = Tally::new();
    let n = plan.reqs.len();
    let mut conn: Option<http1::SendRequest<Full<Bytes>>> = None;
    let mut target: Option<(String, http1::SendRequest<Full<Bytes>>)> = None;
    loop {
        let mut fed_slot: Option<FedReq> = None;
        let idx = if let Some(feed) = &plan.feed {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            if plan.byte_budget.is_some_and(|b| sent_bytes.load(Ordering::Relaxed) >= b) {
                break;
            }
            let received = match deadline {
                Some(d) => match tokio::time::timeout_at(tokio::time::Instant::from_std(d), feed.recv()).await {
                    Ok(r) => r,
                    Err(_) => break,
                },
                None => feed.recv().await,
            };
            match received {
                Ok(item) => {
                    sent_bytes.fetch_add(item.size, Ordering::Relaxed);
                    fed_slot = Some(item);
                    0
                }
                Err(_) => break,
            }
        } else {
            let seq = counter.fetch_add(1, Ordering::Relaxed);
            match plan.mode {
                Mode::Count => {
                    if seq >= n {
                        break;
                    }
                    seq
                }
                Mode::For(_) => {
                    if Instant::now() >= deadline.expect("deadline in duration mode") {
                        break;
                    }
                    match &plan.order {
                        Some(order) => order[seq % order.len()] as usize,
                        None => seq % n,
                    }
                }
            }
        };
        let spec = match &fed_slot {
            Some(f) => &f.spec,
            None => &plan.reqs[idx],
        };
        let mut attempts = 0;
        loop {
            if conn.is_none() {
                match connect(addr).await {
                    Ok(c) => conn = Some(c),
                    Err(_) => {
                        attempts += 1;
                        if attempts >= 3 {
                            tally.io_err += 1;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        continue;
                    }
                }
            }
            let sender = conn.as_mut().expect("connected");
            let started = Instant::now();
            let sent = match sender.ready().await {
                Ok(()) => sender.send_request(spec.build()).await,
                Err(e) => Err(e),
            };
            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    conn = None;
                    attempts += 1;
                    if attempts >= 2 {
                        tally.io_err += 1;
                        if tally.error_sample.is_none() {
                            let mut text = e.to_string();
                            let mut src = std::error::Error::source(&e);
                            while let Some(s) = src {
                                text.push_str(&format!(": {s}"));
                                src = s.source();
                            }
                            tally.error_sample = Some(format!("connection error: {text}"));
                        }
                        break;
                    }
                    continue;
                }
            };
            let Some(mut hop) = read_response(resp).await else {
                conn = None;
                tally.io_err += 1;
                break;
            };
            if hop.closing {
                conn = None;
            }
            if spec.follow_redirects && (300..400).contains(&hop.status) {
                let Some(location) = hop.location.clone() else {
                    tally.total += 1;
                    *tally.status.entry(hop.status).or_insert(0) += 1;
                    if tally.error_sample.is_none() {
                        tally.error_sample = Some(format!("{}: redirect without a Location header", hop.status));
                    }
                    break;
                };
                let Some(next) = follow(&mut target, &location).await else {
                    tally.io_err += 1;
                    if tally.error_sample.is_none() {
                        tally.error_sample = Some(format!("{}: could not follow redirect to {location}", hop.status));
                    }
                    break;
                };
                hop = next;
            }
            let elapsed = started.elapsed();
            let (status, got, head) = (hop.status, hop.got, hop.head);
            tally.total += 1;
            tally.bytes_in += got;
            *tally.status.entry(status).or_insert(0) += 1;
            let status_ok = status >= spec.ok_lo && status <= spec.ok_hi;
            let len_ok = spec.expect_len.is_none_or(|l| l == got);
            if !status_ok && tally.error_sample.is_none() {
                let text = String::from_utf8_lossy(&head).replace(['\n', '\r'], " ");
                tally.error_sample = Some(format!("{status}: {}", text.trim()));
            }
            if status_ok && !len_ok {
                tally.bad_len += 1;
            }
            if status_ok && len_ok {
                tally.ok += 1;
                tally.bytes_out_ok += spec.body.len() as u64;
                if let Some(f) = &fed_slot {
                    if kept_count.load(Ordering::Relaxed) < plan.keep && kept_count.fetch_add(1, Ordering::Relaxed) < plan.keep {
                        tally.kept.push((f.hash.clone(), f.size));
                    }
                } else if matches!(plan.mode, Mode::Count) {
                    tally.ok_idx.push(idx as u32);
                }
            }
            tally.hist.saturating_record(elapsed.as_micros().max(1) as u64);
            break;
        }
    }
    tally
}

pub async fn run_for(addr: SocketAddr, plan: Plan, duration: Duration) -> Outcome {
    let feed = plan.feed.clone();
    let out = run_inner(addr, plan, Some(duration)).await;
    if let Some(f) = feed {
        f.close();
    }
    out
}

pub async fn run(addr: SocketAddr, plan: Plan) -> Outcome {
    run_inner(addr, plan, None).await
}

async fn run_inner(addr: SocketAddr, plan: Plan, fed_deadline: Option<Duration>) -> Outcome {
    let plan = Arc::new(plan);
    let counter = Arc::new(AtomicUsize::new(0));
    let kept_count = Arc::new(AtomicUsize::new(0));
    let sent_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let started = Instant::now();
    let deadline = match (plan.mode, fed_deadline) {
        (_, Some(d)) => Some(started + d),
        (Mode::For(d), None) => Some(started + d),
        (Mode::Count, None) => None,
    };
    let mut handles = Vec::new();
    for _ in 0..plan.connections {
        handles.push(tokio::spawn(worker(addr, plan.clone(), counter.clone(), kept_count.clone(), sent_bytes.clone(), deadline)));
    }
    let mut out = Outcome {
        wall: Duration::ZERO,
        total: 0,
        ok: 0,
        io_err: 0,
        bad_len: 0,
        bytes_in: 0,
        bytes_out_ok: 0,
        status: BTreeMap::new(),
        hist: Histogram::new_with_bounds(1, 120_000_000, 3).expect("histogram bounds"),
        ok_idx: Vec::new(),
        kept: Vec::new(),
        error_sample: None,
    };
    for h in handles {
        let t = h.await.expect("worker task");
        out.total += t.total;
        out.ok += t.ok;
        out.io_err += t.io_err;
        out.bad_len += t.bad_len;
        out.bytes_in += t.bytes_in;
        out.bytes_out_ok += t.bytes_out_ok;
        for (k, v) in t.status {
            *out.status.entry(k).or_insert(0) += v;
        }
        out.hist.add(&t.hist).expect("merge histograms");
        out.ok_idx.extend(t.ok_idx);
        out.kept.extend(t.kept);
        if out.error_sample.is_none() {
            out.error_sample = t.error_sample;
        }
    }
    out.wall = started.elapsed();
    out.ok_idx.sort_unstable();
    out
}

pub async fn fetch(addr: SocketAddr, path: &str, follow_redirects: bool) -> Result<(u16, Vec<u8>)> {
    let mut sender = connect(addr).await?;
    let req = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header("host", addr.to_string())
        .body(Full::new(Bytes::new()))?;
    let resp = sender.send_request(req).await?;
    let status = resp.status().as_u16();
    let location = resp.headers().get(hyper::header::LOCATION).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let body = resp.into_body().collect().await?.to_bytes();
    if follow_redirects && (300..400).contains(&status) {
        let location = location.ok_or_else(|| anyhow::anyhow!("redirect without a Location header"))?;
        let (host, next) = split_location(&location).ok_or_else(|| anyhow::anyhow!("cannot follow {location}"))?;
        let target = tokio::net::lookup_host(host.as_str()).await?.next().ok_or_else(|| anyhow::anyhow!("cannot resolve {host}"))?;
        return Box::pin(fetch(target, &next, false)).await;
    }
    Ok((status, body.to_vec()))
}

pub async fn wait_ready(addr: SocketAddr, path: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if fetch(addr, path, false).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("{addr} did not answer within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;

    async fn serve() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| async move {
                        let port = addr.port();
                        let (status, body) = match req.uri().path() {
                            "/ok" => (200, Bytes::from_static(b"hello")),
                            "/short" => (200, Bytes::from_static(b"hi")),
                            "/put" => (201, Bytes::new()),
                            "/redir-ok" | "/redir-short" | "/redir-gone" | "/redir-loop" => (302, Bytes::new()),
                            _ => (404, Bytes::new()),
                        };
                        let mut resp = hyper::Response::builder().status(status);
                        match req.uri().path() {
                            "/redir-ok" => resp = resp.header("location", format!("http://127.0.0.1:{port}/ok")),
                            "/redir-short" => resp = resp.header("location", format!("http://127.0.0.1:{port}/short")),
                            "/redir-gone" => resp = resp.header("location", format!("http://127.0.0.1:{port}/gone")),
                            "/redir-loop" => resp = resp.header("location", format!("http://127.0.0.1:{port}/redir-loop")),
                            _ => {}
                        }
                        Ok::<_, Infallible>(resp.body(Full::new(body)).unwrap())
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        addr
    }

    fn spec(path: &str, method: Method, lo: u16, hi: u16, len: Option<u64>, body: &'static [u8]) -> ReqSpec {
        ReqSpec {
            method,
            uri: path.parse().unwrap(),
            headers: HeaderMap::new(),
            body: Bytes::from_static(body),
            ok_lo: lo,
            ok_hi: hi,
            expect_len: len,
            follow_redirects: false,
            resign: None,
        }
    }

    fn following(path: &str, len: Option<u64>) -> ReqSpec {
        ReqSpec { follow_redirects: true, ..spec(path, Method::GET, 200, 200, len, b"") }
    }

    #[tokio::test]
    async fn counts_each_request_once_and_records_ok_indexes() {
        let addr = serve().await;
        let reqs = (0..50).map(|_| spec("/put", Method::PUT, 200, 201, None, b"abc")).collect();
        let out = run(addr, Plan { reqs, order: None, mode: Mode::Count, connections: 8, feed: None, keep: 0, byte_budget: None }).await;
        assert_eq!(out.total, 50);
        assert_eq!(out.ok, 50);
        assert_eq!(out.ok_idx, (0..50).collect::<Vec<u32>>());
        assert_eq!(out.bytes_out_ok, 150);
        assert_eq!(out.status[&201], 50);
    }

    #[tokio::test]
    async fn wrong_status_and_short_bodies_are_not_ok() {
        let addr = serve().await;
        let reqs = vec![
            spec("/ok", Method::GET, 200, 200, Some(5), b""),
            spec("/short", Method::GET, 200, 200, Some(5), b""),
            spec("/gone", Method::GET, 200, 200, None, b""),
        ];
        let out = run(addr, Plan { reqs, order: None, mode: Mode::Count, connections: 2, feed: None, keep: 0, byte_budget: None }).await;
        assert_eq!(out.total, 3);
        assert_eq!(out.ok, 1);
        assert_eq!(out.bad_len, 1);
        assert_eq!(out.status[&404], 1);
    }

    #[tokio::test]
    async fn a_redirect_is_not_ok_unless_followed_to_the_right_bytes() {
        let addr = serve().await;
        let plain = vec![spec("/redir-ok", Method::GET, 200, 200, Some(5), b"")];
        let out = run(addr, Plan { reqs: plain, order: None, mode: Mode::Count, connections: 1, feed: None, keep: 0, byte_budget: None }).await;
        assert_eq!((out.ok, out.status[&302]), (0, 1));

        let reqs = vec![
            following("/redir-ok", Some(5)),
            following("/redir-short", Some(5)),
            following("/redir-gone", Some(5)),
        ];
        let out = run(addr, Plan { reqs, order: None, mode: Mode::Count, connections: 1, feed: None, keep: 0, byte_budget: None }).await;
        assert_eq!(out.total, 3);
        assert_eq!(out.ok, 1);
        assert_eq!(out.bad_len, 1);
        assert_eq!(out.status[&200], 2);
        assert_eq!(out.status[&404], 1);
    }

    #[tokio::test]
    async fn a_redirect_loop_does_not_count_as_a_success() {
        let addr = serve().await;
        let reqs = vec![following("/redir-loop", Some(5))];
        let out = run(addr, Plan { reqs, order: None, mode: Mode::Count, connections: 1, feed: None, keep: 0, byte_budget: None }).await;
        assert_eq!(out.ok, 0);
    }

    #[tokio::test]
    async fn fetch_follows_a_redirect_to_the_body() {
        let addr = serve().await;
        assert_eq!(fetch(addr, "/redir-ok", true).await.unwrap(), (200, b"hello".to_vec()));
        assert_eq!(fetch(addr, "/redir-ok", false).await.unwrap().0, 302);
    }

    fn fed(path: &str, n: usize) -> (async_channel::Receiver<FedReq>, async_channel::Sender<FedReq>) {
        let (tx, rx) = async_channel::bounded(1024);
        for i in 0..n {
            let _ = tx.try_send(FedReq {
                spec: spec(path, Method::PUT, 200, 201, None, b"abc"),
                hash: format!("h{i}"),
                size: 3,
            });
        }
        (rx, tx)
    }

    #[tokio::test]
    async fn a_fed_upload_stops_at_the_deadline_and_remembers_up_to_keep_blobs() {
        let addr = serve().await;
        let (rx, _tx) = fed("/put", 200);
        let plan = Plan { reqs: Vec::new(), order: None, mode: Mode::Count, connections: 4, feed: Some(rx), keep: 50, byte_budget: None };
        let started = Instant::now();
        let out = run_for(addr, plan, Duration::from_millis(300)).await;
        assert!(started.elapsed() < Duration::from_secs(2), "must stop near the deadline");
        assert_eq!(out.ok, 200);
        assert_eq!(out.kept.len(), 50);
        assert!(out.kept.iter().all(|(h, size)| h.starts_with('h') && *size == 3));
    }

    #[tokio::test]
    async fn a_fed_upload_with_no_more_work_ends_before_the_deadline() {
        let addr = serve().await;
        let (rx, tx) = fed("/put", 10);
        drop(tx);
        let plan = Plan { reqs: Vec::new(), order: None, mode: Mode::Count, connections: 2, feed: Some(rx), keep: 5, byte_budget: None };
        let started = Instant::now();
        let out = run_for(addr, plan, Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(out.ok, 10);
    }

    #[tokio::test]
    async fn duration_mode_runs_until_the_deadline() {
        let addr = serve().await;
        let reqs = vec![spec("/ok", Method::GET, 200, 200, Some(5), b"")];
        let out = run(
            addr,
            Plan { reqs, order: None, mode: Mode::For(Duration::from_millis(300)), connections: 4, feed: None, keep: 0, byte_budget: None },
        )
        .await;
        assert!(out.ok > 100, "only {} requests", out.ok);
        assert!(out.wall >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn dead_server_counts_io_errors_not_a_hang() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let reqs = (0..3).map(|_| spec("/ok", Method::GET, 200, 200, None, b"")).collect();
        let out = run(addr, Plan { reqs, order: None, mode: Mode::Count, connections: 2, feed: None, keep: 0, byte_budget: None }).await;
        assert_eq!(out.ok, 0);
        assert_eq!(out.io_err, 3);
    }

    #[tokio::test]
    async fn fetch_returns_status_and_body() {
        let addr = serve().await;
        assert_eq!(fetch(addr, "/ok", false).await.unwrap(), (200, b"hello".to_vec()));
        assert_eq!(fetch(addr, "/x", false).await.unwrap().0, 404);
    }
}
