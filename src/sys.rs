use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};

pub fn parse_cpus(spec: &str) -> Result<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in spec.split(',').filter(|p| !p.trim().is_empty()) {
        match part.trim().split_once('-') {
            Some((a, b)) => cpus.extend(a.parse::<usize>()?..=b.parse::<usize>()?),
            None => cpus.push(part.trim().parse()?),
        }
    }
    if cpus.is_empty() {
        bail!("empty cpu list {spec:?}");
    }
    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

pub fn set_affinity(cpus: &[usize]) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus {
            libc::CPU_SET(c, &mut set);
        }
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn require_sync_mount(path: &std::path::Path) -> Result<()> {
    let real = std::fs::canonicalize(path)?;
    let info = std::fs::read_to_string("/proc/self/mountinfo")?;
    let best = info
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            let mount = f.get(4)?;
            let opts = f.get(5)?;
            let after = f.iter().position(|x| *x == "-")?;
            let super_opts = f.get(after + 3)?;
            real.starts_with(mount).then(|| (mount.len(), format!("{opts},{super_opts}")))
        })
        .max_by_key(|(len, _)| *len);
    match best {
        Some((_, opts)) if opts.split(',').any(|o| o == "sync" || o == "dirsync") => Ok(()),
        Some((_, opts)) => bail!("{} is not on a filesystem mounted with sync ({opts}); run scripts/syncfs.sh up", real.display()),
        None => bail!("cannot find the mount of {}", real.display()),
    }
}

pub fn online_cpus() -> usize {
    unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) as usize }
}

fn clock_ticks() -> f64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

struct ProcInfo {
    ppid: u32,
    ticks: u64,
}

fn read_stat(pid: u32) -> Option<ProcInfo> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(ProcInfo {
        ppid: f.get(1)?.parse().ok()?,
        ticks: f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?,
    })
}

fn snapshot() -> HashMap<u32, ProcInfo> {
    let mut all = HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for e in dir.flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) {
                if let Some(info) = read_stat(pid) {
                    all.insert(pid, info);
                }
            }
        }
    }
    all
}

fn tree(all: &HashMap<u32, ProcInfo>, roots: &[u32]) -> Vec<u32> {
    let mut found: Vec<u32> = roots.iter().copied().filter(|r| all.contains_key(r)).collect();
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        for (pid, info) in all {
            if info.ppid == parent && !found.contains(pid) {
                found.push(*pid);
            }
        }
        i += 1;
    }
    found
}

pub fn cpu_seconds(roots: &[u32]) -> f64 {
    let all = snapshot();
    let ticks: u64 = tree(&all, roots).iter().filter_map(|p| all.get(p)).map(|i| i.ticks).sum();
    ticks as f64 / clock_ticks()
}

pub fn self_cpu_seconds() -> f64 {
    read_stat(std::process::id()).map_or(0.0, |i| i.ticks as f64 / clock_ticks())
}

#[derive(Clone, Copy, Default)]
pub struct Mem {
    pub anon: u64,
    pub file: u64,
}

fn read_mem(pid: u32) -> Mem {
    let mut m = Mem::default();
    if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in text.lines() {
            let kb = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
            if line.starts_with("RssAnon:") {
                m.anon = kb(line);
            } else if line.starts_with("RssFile:") {
                m.file = kb(line);
            }
        }
    }
    m
}

pub fn mem_now(roots: &[u32]) -> Mem {
    let all = snapshot();
    let mut total = Mem::default();
    for pid in tree(&all, roots) {
        let m = read_mem(pid);
        total.anon += m.anon;
        total.file += m.file;
    }
    total
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    anon: Arc<AtomicU64>,
    file: Arc<AtomicU64>,
    handle: tokio::task::JoinHandle<()>,
}

impl Sampler {
    pub fn start(roots: Vec<u32>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let anon = Arc::new(AtomicU64::new(0));
        let file = Arc::new(AtomicU64::new(0));
        let (s, a, f) = (stop.clone(), anon.clone(), file.clone());
        let handle = tokio::spawn(async move {
            while !s.load(Ordering::Relaxed) {
                let roots = roots.clone();
                if let Ok(m) = tokio::task::spawn_blocking(move || mem_now(&roots)).await {
                    a.fetch_max(m.anon, Ordering::Relaxed);
                    f.fetch_max(m.file, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
        Self { stop, anon, file, handle }
    }

    pub async fn finish(self) -> Mem {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.await;
        Mem {
            anon: self.anon.load(Ordering::Relaxed),
            file: self.file.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plain_tmp_is_not_a_sync_mount() {
        let dir = std::env::temp_dir();
        assert!(super::require_sync_mount(&dir).is_err());
    }

    use super::*;

    #[test]
    fn parses_ranges_and_lists() {
        assert_eq!(parse_cpus("0-3,8,10-11").unwrap(), vec![0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(parse_cpus("2,2,1").unwrap(), vec![1, 2]);
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert!(parse_cpus("").is_err());
        assert!(parse_cpus("a-b").is_err());
    }

    #[test]
    fn own_process_is_measurable() {
        let me = std::process::id();
        assert!(cpu_seconds(&[me]) >= 0.0);
        assert!(mem_now(&[me]).anon > 0);
    }
}
