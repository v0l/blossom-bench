use std::net::SocketAddr;
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

pub async fn execute(ctx: &mut Ctx<'_>, w: &Workload, seed: u64) -> Result<WorkloadResult> {
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
    };

    let measured_mode;
    let mut build: Box<dyn FnMut(Mode) -> Plan>;
    match w.kind {
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
                Plan { reqs, order, mode, connections: conns }
            });
        }
    }

    let mut verified = (0, 0);
    if matches!(w.kind, Kind::Get) && !plan_blobs.is_empty() {
        verified = verify(ctx, &plan_blobs, &mut rng).await;
    }

    if let Mode::For(_) = measured_mode {
        let warm = Duration::from_secs(ctx.run.warmup_s);
        if !warm.is_zero() {
            load::run(ctx.addr, build(Mode::For(warm))).await;
        }
    }

    let sampler = Sampler::start(ctx.roots.clone());
    let cpu0 = sys::cpu_seconds(&ctx.roots);
    let self0 = sys::self_cpu_seconds();
    let out: Outcome = load::run(ctx.addr, build(measured_mode)).await;
    let cpu1 = sys::cpu_seconds(&ctx.roots);
    let self1 = sys::self_cpu_seconds();
    let mem = sampler.finish().await;

    let secs = out.wall.as_secs_f64();
    let bytes = if w.kind == Kind::Upload { out.bytes_out_ok } else { out.bytes_in };
    let cpu_s = (cpu1 - cpu0).max(0.0);

    if w.kind == Kind::Upload {
        if ctx.def.upload.settle_s > 0 {
            tokio::time::sleep(Duration::from_secs(ctx.def.upload.settle_s)).await;
        }
        let kept: Vec<Blob> = out.ok_idx.iter().map(|&i| upload_all[i as usize].clone()).collect();
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
        disqualified: offload.as_ref().is_some_and(|o| o.disqualify),
        offloaded: offload.map(|o| o.reason),
        error_sample: out.error_sample,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
