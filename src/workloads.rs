use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue};
use hyper::{Method, Uri};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Kind, ServerDef, SuiteRun, Workload};
use crate::load::{self, Mode, Outcome, Plan, ReqSpec};
use crate::nostr::{Signer, random_bytes};
use crate::sys::{self, Sampler};

#[derive(Clone)]
pub struct Blob {
    pub hash: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct WorkloadResult {
    pub name: String,
    pub kind: String,
    pub size: Option<u64>,
    pub connections: usize,
    pub requests: u64,
    pub ok: u64,
    pub io_errors: u64,
    pub bad_length: u64,
    pub status: std::collections::BTreeMap<u16, u64>,
    pub seconds: f64,
    pub ok_per_s: f64,
    pub mb_per_s: f64,
    pub p50_us: u64,
    pub p90_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub max_us: u64,
    pub cpu_cores: f64,
    pub ok_per_cpu_s: f64,
    pub peak_anon_mb: f64,
    pub peak_file_mb: f64,
    pub load_gen_util: f64,
    pub verified: usize,
    pub verify_bad: usize,
    pub skipped: Option<String>,
    #[serde(default)]
    pub capped: bool,
    #[serde(default)]
    pub seed_missing: usize,
    #[serde(default)]
    pub disqualified: bool,
    #[serde(default)]
    pub offloaded: Option<String>,
    #[serde(default)]
    pub error_sample: Option<String>,
}

pub struct Ctx<'a> {
    pub def: &'a ServerDef,
    pub run: &'a SuiteRun,
    pub addr: SocketAddr,
    pub roots: Vec<u32>,
    pub signer: Signer,
    pub load_cpus: usize,
    pub blobs: std::collections::HashMap<String, Vec<Blob>>,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn make_content(rng: &mut Rng, size: usize, kind: &str, index: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    if kind == "text" {
        let words = [
            "blossom", "server", "nostr", "blob", "relay", "upload", "media", "content", "address", "hash",
        ];
        let mut out = String::with_capacity(size + 16);
        out.push_str(&format!("blob {index} {}\n", rng.next()));
        while out.len() < size {
            out.push_str(words[(rng.next() % words.len() as u64) as usize]);
            out.push(' ');
        }
        data.copy_from_slice(&out.as_bytes()[..size]);
        return data;
    }
    for chunk in data.chunks_mut(8) {
        let v = rng.next().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    if kind == "png" {
        const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let n = PNG_MAGIC.len().min(data.len());
        data[..n].copy_from_slice(&PNG_MAGIC[..n]);
    }
    data
}

const DEFAULT_UPLOAD_BYTES: u64 = 3 << 30;
const WARMUP_UPLOAD_BYTES: u64 = 256 << 20;

struct FeedSpec {
    addr: SocketAddr,
    size: usize,
    content_kind: String,
    content_type: String,
    path: String,
    auth: String,
    sha_header: bool,
    resign: bool,
    host: String,
    bound: Option<String>,
    signers: usize,
}

struct FeedStop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for FeedStop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn build_fed(spec: &FeedSpec, signer: &Signer, rng: &mut Rng, index: usize) -> Option<load::FedReq> {
    let data = make_content(rng, spec.size, &spec.content_kind, index);
    let hash = hex::encode(Sha256::digest(&data));
    let token = signer.upload_token(&hash, &spec.host, &spec.auth, spec.bound.as_deref()).ok()?;
    let mut h = headers(spec.addr);
    h.insert(AUTHORIZATION, HeaderValue::from_str(&token).ok()?);
    h.insert(CONTENT_TYPE, HeaderValue::from_str(&spec.content_type).ok()?);
    if spec.sha_header {
        h.insert("x-sha-256", HeaderValue::from_str(&hash).ok()?);
    }
    let resign: Option<std::sync::Arc<dyn Fn() -> Option<HeaderValue> + Send + Sync>> = if spec.resign {
        let (signer, hash, host, auth, bound) = (signer.clone(), hash.clone(), spec.host.clone(), spec.auth.clone(), spec.bound.clone());
        Some(std::sync::Arc::new(move || {
            let t = signer.upload_token(&hash, &host, &auth, bound.as_deref()).ok()?;
            HeaderValue::from_str(&t).ok()
        }))
    } else {
        None
    };
    Some(load::FedReq {
        spec: ReqSpec {
            method: Method::PUT,
            uri: spec.path.parse::<Uri>().ok()?,
            headers: h,
            body: Bytes::from(data),
            ok_lo: 200,
            ok_hi: 201,
            expect_len: None,
            follow_redirects: false,
            resign,
        },
        hash,
        size: spec.size as u64,
    })
}

fn start_feed(spec: &FeedSpec, seed: u64, _keep: usize, conns: usize) -> (async_channel::Receiver<load::FedReq>, FeedStop) {
    let depth = (conns * 4).max(64);
    let (tx, rx) = async_channel::bounded::<load::FedReq>(depth);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producers = 8usize;
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for p in 0..producers {
        let tx = tx.clone();
        let stop = stop.clone();
        let next = next.clone();
        let spec = FeedSpec {
            addr: spec.addr,
            size: spec.size,
            content_kind: spec.content_kind.clone(),
            content_type: spec.content_type.clone(),
            path: spec.path.clone(),
            auth: spec.auth.clone(),
            sha_header: spec.sha_header,
            resign: spec.resign,
            host: spec.host.clone(),
            bound: spec.bound.clone(),
            signers: spec.signers,
        };
        std::thread::spawn(move || {
            let pool: Vec<Signer> = (0..spec.signers.max(1)).filter_map(|i| Signer::numbered(i).ok()).collect();
            if pool.is_empty() {
                return;
            }
            let mut rng = Rng((seed ^ (p as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15)) | 1);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(item) = build_fed(&spec, &pool[i % pool.len()], &mut rng, i) else {
                    break;
                };
                if tx.send_blocking(item).is_err() {
                    break;
                }
            }
        });
    }
    (rx, FeedStop(stop))
}

fn zipf_order(n: usize, s: f64, rng: &mut Rng) -> Vec<u32> {
    let mut cum = Vec::with_capacity(n);
    let mut acc = 0.0;
    for i in 0..n {
        acc += 1.0 / ((i + 1) as f64).powf(s);
        cum.push(acc);
    }
    (0..1_000_000)
        .map(|_| {
            let u = rng.unit() * acc;
            cum.partition_point(|c| *c < u).min(n - 1) as u32
        })
        .collect()
}

fn headers(addr: SocketAddr) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(HOST, HeaderValue::from_str(&addr.to_string()).expect("host header"));
    h
}

fn pct(h: &hdrhistogram::Histogram<u64>, q: f64) -> u64 {
    if h.is_empty() { 0 } else { h.value_at_quantile(q) }
}

fn skipped(w: &Workload, why: &str) -> WorkloadResult {
    WorkloadResult {
        name: w.name.clone(),
        kind: format!("{:?}", w.kind).to_lowercase(),
        size: w.size,
        skipped: Some(why.to_string()),
        ..Default::default()
    }
}

async fn verify(ctx: &Ctx<'_>, blobs: &[Blob], rng: &mut Rng) -> (usize, usize) {
    let mut bad = 0;
    let n = ctx.run.verify.min(blobs.len());
    for _ in 0..n {
        let b = &blobs[(rng.next() % blobs.len() as u64) as usize];
        let ok = match load::fetch(ctx.addr, &format!("/{}", b.hash), ctx.def.follow_redirects).await {
            Ok((200, body)) => hex::encode(Sha256::digest(&body)) == b.hash && body.len() as u64 == b.size,
            _ => false,
        };
        if !ok {
            bad += 1;
        }
    }
    (n, bad)
}

const SEED_RETRIES: usize = 20;
const SETTLE_LIMIT: Duration = Duration::from_secs(900);

async fn settle(ctx: &Ctx<'_>, blobs: &[Blob]) {
    tokio::time::sleep(Duration::from_secs(ctx.def.upload.settle_s)).await;
    let Some(last) = blobs.last() else { return };
    let started = std::time::Instant::now();
    while started.elapsed() < SETTLE_LIMIT {
        if let Ok((200, body)) = load::fetch(ctx.addr, &format!("/{}", last.hash), ctx.def.follow_redirects).await {
            if hex::encode(Sha256::digest(&body)) == last.hash {
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn seed(ctx: &mut Ctx<'_>, w: &Workload) -> Result<WorkloadResult> {
    let size = w.size.expect("validated") as usize;
    let count = w.count.expect("validated");
    let conns = ctx.def.upload.max_connections.map_or(ctx.run.connections, |c| c.min(ctx.run.connections));
    let content_kind = ctx.def.upload.content.as_deref().unwrap_or(&w.content).to_string();
    let content_type = match &ctx.def.upload.content_type {
        Some(c) => c.clone(),
        None if content_kind == "png" => "image/png".into(),
        None if content_kind == "text" => "text/plain".into(),
        None => "application/octet-stream".into(),
    };
    let spec = FeedSpec {
        addr: ctx.addr,
        size,
        content_kind,
        content_type,
        path: ctx.def.upload.path.clone(),
        auth: ctx.def.upload.auth.clone(),
        sha_header: ctx.def.upload.sha_header,
        resign: true,
        host: ctx.addr.ip().to_string(),
        bound: ctx.def.upload.bound_auth.then(|| format!("http://{}", ctx.addr)),
        signers: ctx.def.upload.signers.max(1),
    };
    let pool: Vec<Signer> = (0..spec.signers).map(Signer::numbered).collect::<Result<_>>()?;
    let seed_value = seed_of(&w.name);
    let mut rng = Rng(seed_value | 1);
    let mut pending: Vec<load::FedReq> = Vec::with_capacity(count);
    for i in 0..count {
        let item = build_fed(&spec, &pool[i % pool.len()], &mut rng, i).ok_or_else(|| anyhow::anyhow!("could not build seed blob {i}"))?;
        pending.push(item);
    }
    let started = std::time::Instant::now();
    let mut stored: Vec<Blob> = Vec::with_capacity(count);
    let mut attempts = 0usize;
    let mut last_error = None;
    let mut rounds = 0usize;
    while !pending.is_empty() && rounds < SEED_RETRIES {
        rounds += 1;
        let blobs: Vec<Blob> = pending.iter().map(|f| Blob { hash: f.hash.clone(), size: f.size }).collect();
        let reqs: Vec<ReqSpec> = pending
            .iter()
            .map(|f| ReqSpec {
                method: f.spec.method.clone(),
                uri: f.spec.uri.clone(),
                headers: f.spec.headers.clone(),
                body: f.spec.body.clone(),
                ok_lo: 200,
                ok_hi: 201,
                expect_len: None,
                follow_redirects: false,
                resign: f.spec.resign.clone(),
            })
            .collect();
        attempts += reqs.len();
        let out = load::run(ctx.addr, Plan { reqs, order: None, mode: Mode::Count, connections: conns, feed: None, keep: 0, byte_budget: None }).await;
        if out.error_sample.is_some() {
            last_error = out.error_sample.clone();
        }
        let ok: std::collections::HashSet<u32> = out.ok_idx.iter().copied().collect();
        for (i, b) in blobs.into_iter().enumerate() {
            if ok.contains(&(i as u32)) {
                stored.push(b);
            }
        }
        let mut idx = 0u32;
        pending.retain(|_| {
            let keep = !ok.contains(&idx);
            idx += 1;
            keep
        });
        if !pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(250 * rounds as u64)).await;
        }
    }
    if ctx.def.upload.settle_s > 0 {
        settle(ctx, &stored).await;
    }
    let mut vrng = Rng(seed_value ^ 0xBEEF | 1);
    let verified = verify(ctx, &stored, &mut vrng).await;
    let secs = started.elapsed().as_secs_f64();
    let missing = count - stored.len();
    ctx.blobs.insert(w.name.clone(), stored);
    Ok(WorkloadResult {
        name: w.name.clone(),
        kind: "seed".into(),
        size: w.size,
        connections: conns,
        requests: attempts as u64,
        ok: (count - missing) as u64,
        seconds: secs,
        verified: verified.0,
        verify_bad: verified.1,
        seed_missing: missing,
        error_sample: if missing > 0 { last_error } else { None },
        ..Default::default()
    })
}

fn seed_of(name: &str) -> u64 {
    let h = Sha256::digest(format!("blossom-bench seed {name}").as_bytes());
    u64::from_le_bytes(h[..8].try_into().expect("8 bytes"))
}

pub async fn execute(ctx: &mut Ctx<'_>, w: &Workload, seed_arg: u64) -> Result<WorkloadResult> {
    if w.kind == Kind::Seed {
        return seed(ctx, w).await;
    }
    let seed = seed_arg;
    let offload = ctx.def.offload(w.kind).cloned();
    let mut rng = Rng(seed ^ 0x9E3779B97F4A7C15 | 1);
    let mut conns = w.connections.unwrap_or(ctx.run.connections);
    if w.kind == Kind::Upload {
        if let Some(cap) = ctx.def.upload.max_connections {
            conns = conns.min(cap);
        }
    }
    let host = ctx.addr.ip().to_string();
    let follow_redirects = ctx.def.follow_redirects;
    let bound = ctx.def.upload.bound_auth.then(|| format!("http://{}", ctx.addr));

    let (mut plan_blobs, mut upload_all): (Vec<Blob>, Vec<Blob>) = (Vec::new(), Vec::new());
    let plan = |mode: Mode, reqs: Vec<ReqSpec>, order: Option<Vec<u32>>| Plan {
        reqs,
        order,
        mode,
        connections: conns,
        feed: None,
        keep: 0,
        byte_budget: None,
    };

    let measured_mode;
    let mut timed_upload: Option<(Plan, Duration, FeedStop)> = None;
    let mut build: Box<dyn FnMut(Mode) -> Plan>;
    match w.kind {
        Kind::Upload if w.duration_s.is_some() => {
            let size = w.size.expect("validated") as usize;
            let dur = Duration::from_secs(w.duration_s.unwrap_or(ctx.run.duration_s));
            let content_kind = ctx.def.upload.content.as_deref().unwrap_or(&w.content).to_string();
            let content_type = match &ctx.def.upload.content_type {
                Some(c) => c.clone(),
                None if content_kind == "png" => "image/png".into(),
                None if content_kind == "text" => "text/plain".into(),
                None => "application/octet-stream".into(),
            };
            let spec = FeedSpec {
                addr: ctx.addr,
                size,
                content_kind,
                content_type,
                path: ctx.def.upload.path.clone(),
                auth: ctx.def.upload.auth.clone(),
                sha_header: ctx.def.upload.sha_header,
                resign: ctx.def.upload.resign,
                host: host.clone(),
                bound: bound.clone(),
                signers: ctx.def.upload.signers.max(1),
            };
            let keep_blobs = w.count.unwrap_or(2000).max(ctx.run.verify.max(1));
            let warm = Duration::from_secs(ctx.run.warmup_s);
            if !warm.is_zero() {
                let (feed, stop) = start_feed(&spec, seed ^ 0x77, 0, conns);
                let mut p = plan(Mode::Count, Vec::new(), None);
                p.feed = Some(feed);
                p.byte_budget = Some(WARMUP_UPLOAD_BYTES.min(w.max_bytes.unwrap_or(DEFAULT_UPLOAD_BYTES) / 8));
                let _ = load::run_for(ctx.addr, p, warm).await;
                drop(stop);
            }
            let (feed, stop) = start_feed(&spec, seed, keep_blobs, conns);
            let mut p = plan(Mode::Count, Vec::new(), None);
            p.feed = Some(feed);
            p.keep = keep_blobs;
            p.byte_budget = Some(w.max_bytes.unwrap_or(DEFAULT_UPLOAD_BYTES));
            timed_upload = Some((p, dur, stop));
            measured_mode = Mode::For(dur);
            build = Box::new(|mode| Plan { reqs: Vec::new(), order: None, mode, connections: 0, feed: None, keep: 0, byte_budget: None });
        }
        Kind::Upload => {
            let size = w.size.expect("validated") as usize;
            let count = w.count.expect("validated");
            if (size as u64).saturating_mul(count as u64) > (6u64 << 30) {
                return Ok(skipped(w, "workload would hold more than 6 GiB in memory"));
            }
            let content_kind = ctx.def.upload.content.as_deref().unwrap_or(&w.content);
            let mut reqs = Vec::with_capacity(count);
            let pool: Vec<Signer> = (0..ctx.def.upload.signers.max(1))
                .map(Signer::numbered)
                .collect::<Result<_>>()?;
            for i in 0..count {
                let signer = &pool[i % pool.len()];
                let data = make_content(&mut rng, size, content_kind, i);
                let hash = hex::encode(Sha256::digest(&data));
                let token = signer.upload_token(&hash, &host, &ctx.def.upload.auth, bound.as_deref())?;
                let mut h = headers(ctx.addr);
                h.insert(AUTHORIZATION, HeaderValue::from_str(&token)?);
                let ct = match &ctx.def.upload.content_type {
                    Some(c) => c.as_str(),
                    None if content_kind == "png" => "image/png",
                    None if content_kind == "text" => "text/plain",
                    None => "application/octet-stream",
                };
                h.insert(CONTENT_TYPE, HeaderValue::from_str(ct)?);
                if ctx.def.upload.sha_header {
                    h.insert("x-sha-256", HeaderValue::from_str(&hash)?);
                }
                let resign: Option<std::sync::Arc<dyn Fn() -> Option<HeaderValue> + Send + Sync>> = if ctx.def.upload.resign {
                    let (signer, hash, host, auth, bound) =
                        (signer.clone(), hash.clone(), host.clone(), ctx.def.upload.auth.clone(), bound.clone());
                    Some(std::sync::Arc::new(move || {
                        let t = signer.upload_token(&hash, &host, &auth, bound.as_deref()).ok()?;
                        HeaderValue::from_str(&t).ok()
                    }))
                } else {
                    None
                };
                reqs.push(ReqSpec {
                    method: Method::PUT,
                    uri: ctx.def.upload.path.parse::<Uri>()?,
                    headers: h,
                    body: Bytes::from(data),
                    ok_lo: 200,
                    ok_hi: 201,
                    expect_len: None,
                    follow_redirects: false,
                    resign,
                });
                upload_all.push(Blob { hash, size: size as u64 });
            }
            measured_mode = Mode::Count;
            let mut slot = Some(reqs);
            build = Box::new(move |mode| plan(mode, slot.take().unwrap_or_default(), None));
        }
        Kind::Seed => unreachable!("a seed returns before the plan is built"),
        Kind::Get | Kind::Head | Kind::Miss => {
            for name in &w.blobs {
                if let Some(v) = ctx.blobs.get(name) {
                    plan_blobs.extend(v.iter().cloned());
                }
            }
            if plan_blobs.is_empty() {
                return Ok(skipped(w, "no blobs were uploaded by the source workload"));
            }
            let dur = Duration::from_secs(w.duration_s.unwrap_or(ctx.run.duration_s));
            measured_mode = Mode::For(dur);
            let kind = w.kind;
            let addr = ctx.addr;
            let zipf = w.zipf;
            let blobs = plan_blobs.clone();
            let mut zrng = Rng(seed ^ 0xA5A5_5A5A_1234_5678 | 1);
            let misses: Vec<String> = (0..4096)
                .map(|_| hex::encode(random_bytes::<32>().unwrap_or([7u8; 32])))
                .collect();
            let order = zipf.map(|s| zipf_order(blobs.len(), s, &mut zrng));
            build = Box::new(move |mode| {
                let reqs = match kind {
                    Kind::Miss => misses
                        .iter()
                        .map(|m| ReqSpec {
                            method: Method::GET,
                            uri: format!("/{m}").parse().expect("uri"),
                            headers: headers(addr),
                            body: Bytes::new(),
                            ok_lo: 404,
                            ok_hi: 404,
                            expect_len: None,
                            follow_redirects: false,
                    resign: None,
                        })
                        .collect(),
                    _ => blobs
                        .iter()
                        .map(|b| ReqSpec {
                            method: if kind == Kind::Head { Method::HEAD } else { Method::GET },
                            uri: format!("/{}", b.hash).parse().expect("uri"),
                            headers: headers(addr),
                            body: Bytes::new(),
                            ok_lo: 200,
                            ok_hi: 200,
                            expect_len: if kind == Kind::Get { Some(b.size) } else { None },
                            follow_redirects: follow_redirects && kind == Kind::Get,
                            resign: None,
                        })
                        .collect(),
                };
                let order = if kind == Kind::Miss { None } else { order.clone() };
                Plan { reqs, order, mode, connections: conns, feed: None, keep: 0, byte_budget: None }
            });
        }
    }

    let mut verified = (0, 0);
    if matches!(w.kind, Kind::Get) && !plan_blobs.is_empty() {
        verified = verify(ctx, &plan_blobs, &mut rng).await;
    }

    if let Mode::For(_) = measured_mode {
        let warm = Duration::from_secs(ctx.run.warmup_s);
        if !warm.is_zero() && timed_upload.is_none() {
            load::run(ctx.addr, build(Mode::For(warm))).await;
        }
    }

    let sampler = Sampler::start(ctx.roots.clone());
    let cpu0 = sys::cpu_seconds(&ctx.roots);
    let self0 = sys::self_cpu_seconds();
    let out: Outcome = match timed_upload.take() {
        Some((p, dur, stop)) => {
            let started = std::time::Instant::now();
            let mut out = load::run_for(ctx.addr, p, dur).await;
            out.wall = started.elapsed();
            drop(stop);
            out
        }
        None => load::run(ctx.addr, build(measured_mode)).await,
    };
    let cpu1 = sys::cpu_seconds(&ctx.roots);
    let self1 = sys::self_cpu_seconds();
    let mem = sampler.finish().await;

    let secs = out.wall.as_secs_f64();
    let bytes = if w.kind == Kind::Upload { out.bytes_out_ok } else { out.bytes_in };
    let cpu_s = (cpu1 - cpu0).max(0.0);

    if w.kind == Kind::Upload {
        if ctx.def.upload.settle_s > 0 {
            let recent: Vec<Blob> = out.kept.iter().map(|(hash, size)| Blob { hash: hash.clone(), size: *size }).collect();
            if recent.is_empty() {
                tokio::time::sleep(Duration::from_secs(ctx.def.upload.settle_s)).await;
            } else {
                settle(ctx, &recent).await;
            }
        }
        let kept: Vec<Blob> = if w.duration_s.is_some() {
            out.kept.iter().map(|(hash, size)| Blob { hash: hash.clone(), size: *size }).collect()
        } else {
            out.ok_idx.iter().map(|&i| upload_all[i as usize].clone()).collect()
        };
        verified = verify(ctx, &kept, &mut rng).await;
        ctx.blobs.insert(w.name.clone(), kept);
    }

    Ok(WorkloadResult {
        name: w.name.clone(),
        kind: format!("{:?}", w.kind).to_lowercase(),
        size: w.size,
        connections: conns,
        requests: out.total + out.io_err,
        ok: out.ok,
        io_errors: out.io_err,
        bad_length: out.bad_len,
        status: out.status,
        seconds: secs,
        ok_per_s: out.ok as f64 / secs,
        mb_per_s: bytes as f64 / secs / 1e6,
        p50_us: pct(&out.hist, 0.50),
        p90_us: pct(&out.hist, 0.90),
        p99_us: pct(&out.hist, 0.99),
        p999_us: pct(&out.hist, 0.999),
        max_us: out.hist.max(),
        cpu_cores: cpu_s / secs,
        ok_per_cpu_s: if cpu_s > 0.0 { out.ok as f64 / cpu_s } else { 0.0 },
        peak_anon_mb: mem.anon as f64 / 1048576.0,
        peak_file_mb: mem.file as f64 / 1048576.0,
        load_gen_util: (self1 - self0) / secs / ctx.load_cpus as f64,
        verified: verified.0,
        verify_bad: verified.1,
        skipped: None,
        capped: w.kind == Kind::Upload && w.duration_s.is_some() && secs + 0.5 < w.duration_s.unwrap_or(0) as f64,
        seed_missing: 0,
        disqualified: offload.as_ref().is_some_and(|o| o.disqualify),
        offloaded: offload.map(|o| o.reason),
        error_sample: out.error_sample,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_spec(size: usize) -> FeedSpec {
        FeedSpec {
            addr: "127.0.0.1:18100".parse().unwrap(),
            size,
            content_kind: "random".into(),
            content_type: "application/octet-stream".into(),
            path: "/upload".into(),
            auth: "std".into(),
            sha_header: false,
            resign: false,
            host: "127.0.0.1".into(),
            bound: None,
            signers: 1,
        }
    }

    #[test]
    fn a_fed_request_is_built_with_its_hash_and_size() {
        let signer = Signer::numbered(0).unwrap();
        let item = build_fed(&feed_spec(1024), &signer, &mut Rng(3), 0).expect("a request");
        assert_eq!(item.size, 1024);
        assert_eq!(item.hash.len(), 64);
        assert_eq!(item.spec.body.len(), 1024);
        assert_eq!(hex::encode(Sha256::digest(&item.spec.body)), item.hash);
        assert!(item.spec.headers.contains_key(AUTHORIZATION));
    }

    #[test]
    fn a_feed_produces_distinct_blobs_and_stops_when_told() {
        let (rx, stop) = start_feed(&feed_spec(256), 7, 0, 4);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            let item = rx.recv_blocking().expect("an item");
            assert!(seen.insert(item.hash));
        }
        drop(stop);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert!(drained <= 64 + 8, "the producers must stop soon after the stop flag");
    }

    #[test]
    fn content_is_deterministic_per_seed_and_distinct_per_index() {
        let a = make_content(&mut Rng(5), 1000, "random", 0);
        let b = make_content(&mut Rng(5), 1000, "random", 0);
        let c = make_content(&mut Rng(6), 1000, "random", 0);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 1000);
    }

    #[test]
    fn text_content_is_compressible_ascii_of_the_requested_size() {
        let t = make_content(&mut Rng(9), 4096, "text", 3);
        assert_eq!(t.len(), 4096);
        assert!(t.iter().all(|b| b.is_ascii()));
        assert!(t.starts_with(b"blob 3 "));
    }

    #[test]
    fn odd_sizes_are_filled_exactly() {
        assert_eq!(make_content(&mut Rng(1), 13, "random", 0).len(), 13);
        assert_eq!(make_content(&mut Rng(1), 0, "random", 0).len(), 0);
    }

    #[test]
    fn zipf_favours_low_ranks() {
        let order = zipf_order(100, 1.1, &mut Rng(42));
        assert_eq!(order.len(), 1_000_000);
        let first_decile = order.iter().filter(|&&i| i < 10).count();
        assert!(first_decile > 500_000, "{first_decile}");
        assert!(order.iter().all(|&i| i < 100));
    }
}
